use std::io::Cursor;
use std::collections::HashMap;
use std::sync::{Arc,Mutex};
use ::xdr_codec::{Pack,Unpack};
use ::bytes::{BufMut, BytesMut};
use ::tokio_io::codec;
use ::tokio_io::{AsyncRead, AsyncWrite};
use ::tokio_io::codec::length_delimited;
use ::tokio_proto::multiplex::{self, RequestId};
use ::request;
use ::futures::{Stream, Sink, Poll, StartSend};
use ::futures::sync::mpsc::{Sender,Receiver};

struct LibvirtCodec;

#[derive(Debug)]
pub struct LibvirtRequest {
    pub stream: Option<Sender<BytesMut>>,
    pub sink: Option<Receiver<BytesMut>>,
    pub header: request::virNetMessageHeader,
    pub payload: BytesMut,
}

#[derive(Debug,Clone)]
pub struct LibvirtResponse {
    pub header: request::virNetMessageHeader,
    pub payload: BytesMut,
}

impl codec::Encoder for LibvirtCodec {
    type Item = (RequestId, LibvirtRequest);
    type Error = ::std::io::Error;

    fn encode(&mut self, msg: (RequestId, LibvirtRequest), buf: &mut BytesMut) -> Result<(), Self::Error> {
        use ::std::io::ErrorKind;
        let mut req = msg.1;
        let buf = {
            let mut writer = buf.writer();
            req.header.serial = msg.0 as u32;
            try!(req.header.pack(&mut writer).map_err(|e| ::std::io::Error::new(ErrorKind::InvalidInput, e.to_string())));
            writer.into_inner()
        };
        buf.reserve(req.payload.len());
        buf.put(req.payload);
        Ok(())
    }
}

impl codec::Decoder for LibvirtCodec {
    type Item = (RequestId, LibvirtResponse);
    type Error = ::std::io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        use ::std::io::ErrorKind;
        let (header, hlen, buf) = {
            let mut reader = Cursor::new(buf);
            let (header, hlen) = try!(request::virNetMessageHeader::unpack(&mut reader)
                                        .map_err(|e| ::std::io::Error::new(ErrorKind::InvalidInput, e.to_string())));
            (header, hlen, reader.into_inner())
        };
        let payload = buf.split_off(hlen);
        Ok(Some((header.serial as RequestId, LibvirtResponse {
            header: header,
            payload: payload,
        })))
    }
}

fn framed_delimited<T, C>(framed: length_delimited::Framed<T>, codec: C) -> FramedTransport<T, C>
    where T: AsyncRead + AsyncWrite, C: codec::Encoder + codec::Decoder
 {
    FramedTransport{ inner: framed, codec: codec }
}

struct FramedTransport<T, C> where T: AsyncRead + AsyncWrite + 'static {
    inner: length_delimited::Framed<T>,
    codec: C,
}

impl<T, C> Stream for FramedTransport<T, C> where
                T: AsyncRead + AsyncWrite, C: codec::Decoder,
                ::std::io::Error: ::std::convert::From<<C as ::tokio_io::codec::Decoder>::Error> {
    type Item = <C as codec::Decoder>::Item;
    type Error = <C as codec::Decoder>::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        use futures::Async;
        let codec = &mut self.codec;
        self.inner.poll().and_then(|async| {
            match async {
                Async::Ready(Some(mut buf)) => {
                    let pkt = try!(codec.decode(&mut buf));
                    Ok(Async::Ready(pkt))
                },
                Async::Ready(None) => {
                    Ok(Async::Ready(None))
                },
                Async::NotReady => {
                    Ok(Async::NotReady)
                }
            }
        }).map_err(|e| e.into())
    }
}

impl<T, C> Sink for FramedTransport<T, C> where
        T: AsyncRead + AsyncWrite + 'static,
        C: codec::Encoder + codec::Decoder,
        ::std::io::Error: ::std::convert::From<<C as ::tokio_io::codec::Encoder>::Error> {
    type SinkItem = <C as codec::Encoder>::Item;
    type SinkError = <C as codec::Encoder>::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        use futures::AsyncSink;
        let codec = &mut self.codec;
        let mut buf = BytesMut::with_capacity(64);
        try!(codec.encode(item, &mut buf));
        assert!(try!(self.inner.start_send(buf)).is_ready());
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete().map_err(|e| e.into())
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        try_ready!(self.poll_complete().map_err(|e| e.into()));
        self.inner.close().map_err(|e| e.into())
    }
}

pub struct LibvirtTransport<T> where T: AsyncRead + AsyncWrite + 'static {
    inner: FramedTransport<T, LibvirtCodec>,
    events: Arc<Mutex<HashMap<i32, ::futures::sync::mpsc::Sender<::request::DomainEvent>>>>,
    /* req.id -> stream */
    streams: Arc<Mutex<HashMap<u64, ::futures::sync::mpsc::Sender<BytesMut>>>>,
    /* req.id -> (stream, procedure) */
    sinks: Arc<Mutex<HashMap<u64, (::futures::sync::mpsc::Receiver<BytesMut>, i32)>>>,
}

impl<T> LibvirtTransport<T> where T: AsyncRead + AsyncWrite + 'static {
    fn process_event(&self, resp: &LibvirtResponse) -> ::std::io::Result<bool> {
        let procedure = unsafe { ::std::mem::transmute(resp.header.proc_ as u16) };
        match procedure {
            request::remote_procedure::REMOTE_PROC_DOMAIN_EVENT_CALLBACK_LIFECYCLE => {
                let msg = {
                    let mut cursor = Cursor::new(&resp.payload);
                    let (msg, _) = request::generated::remote_domain_event_callback_lifecycle_msg::unpack(&mut cursor).unwrap();
                    debug!("LIFECYCLE EVENT (CALLBACK) PL: {:?}", msg);
                    msg
                };
                {
                    let mut map = self.events.lock().unwrap();
                    if let Some(sender) = map.get_mut(&msg.callbackID) {
                        use std::io::ErrorKind;
                        try!(sender.start_send(msg.into()).map_err(|e| ::std::io::Error::new(ErrorKind::InvalidInput, e.to_string())));
                        try!(sender.poll_complete().map_err(|e| ::std::io::Error::new(ErrorKind::InvalidInput, e.to_string())));
                    }
                }
                return Ok(true);
            },
            _ => {
                debug!("unknown procedure {:?} in {:?}", procedure, resp);
            },
        }
        Ok(false)
    }

    fn process_sinks(&mut self) {
        use futures::Async;
        let mut sinks_to_drop = Vec::new();

        let mut sinks = self.sinks.lock().unwrap();
        debug!("PROCESSING SINKS: count {}", sinks.len());
         
        for (req_id, &mut (ref mut sink, proc_)) in sinks.iter_mut() {
            'out: loop {
                match sink.poll() {
                    Ok(Async::Ready(Some(buf))) => {
                        let req = LibvirtRequest {
                            stream: None,
                            sink: None,
                            header: request::virNetMessageHeader {
                                type_: ::request::generated::virNetMessageType::VIR_NET_STREAM,
                                status: request::virNetMessageStatus::VIR_NET_CONTINUE,
                                proc_: proc_,
                                ..Default::default()
                            },
                            payload: buf,
                        };
                        debug!("Sink sending {:?}", req.header);
                        self.inner.start_send((*req_id, req));
                    },
                    Ok(Async::Ready(None)) => {
                        sinks_to_drop.push(*req_id);
                        let req = LibvirtRequest {
                            stream: None,
                            sink: None,
                            header: request::virNetMessageHeader {
                                type_: ::request::generated::virNetMessageType::VIR_NET_STREAM,
                                status: request::virNetMessageStatus::VIR_NET_OK,
                                proc_: proc_,
                                ..Default::default()
                            },
                            payload: BytesMut::new(),
                        };
                        debug!("Empty sink, sending empty msg");
                        self.inner.start_send((*req_id, req));
                        break 'out;
                    }
                    _ => {
                        break 'out;
                    },
                }
            }
        }

        for id in sinks_to_drop {
            sinks.remove(&id);
        }
    }

    fn process_stream(&self, resp: LibvirtResponse) {
        debug!("incoming stream: {:?}", resp.header);
        {
            let req_id = resp.header.serial as u64;
            let mut streams = self.streams.lock().unwrap();
            let mut remove_stream = false;

            if let Some(ref mut stream) = streams.get_mut(&req_id) {
                debug!("found stream for request id {}: {:?}", req_id, resp.header);
                let sender = stream;
                if resp.payload.len() != 0 {
                    let _ = sender.start_send(resp.payload);
                    let _ = sender.poll_complete();
                } else {
                    debug!("closing stream {}", req_id);
                    let _ = sender.close();
                    let _ = sender.poll_complete();
                    remove_stream = true;
                }
            } else {
                error!("can't find stream for request id {}: {:?}", req_id, resp.header);
                if resp.header.status == request::generated::virNetMessageStatus::VIR_NET_ERROR {
                    let mut reader = Cursor::new(resp.payload);
                    let (err, _) = request::virNetMessageError::unpack(&mut reader).unwrap();
                    println!("ERROR: {:?}", err);
                }
            }
            if remove_stream {
                streams.remove(&req_id);
            }
        }
    }
}

impl<T> Stream for LibvirtTransport<T> where
    T: AsyncRead + AsyncWrite + 'static,
 {
    type Item = (RequestId, LibvirtResponse);
    type Error = ::std::io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        use futures::Async;
        self.process_sinks();
        match self.inner.poll() {
            Ok(async) => {
                match async {
                Async::Ready(Some((id, resp))) => {
                    debug!("FRAME READY ID: {} RESP: {:?}", id, resp);
                    if try!(self.process_event(&resp)) {
                            debug!("processed event, get next packet");
                            return self.poll();
                    }

                    if resp.header.type_ == request::generated::virNetMessageType::VIR_NET_STREAM {
                        self.process_stream(resp);
                        debug!("processed stream msg, get next packet");
                        return self.poll();
                    }

                    return Ok(Async::Ready(Some((id, resp))));
                },
                _ => debug!("{:?}", async),
                }
                debug!("RETURNING {:?}", async);
                Ok(async)
            },
            Err(e) => Err(e),
        }
    }
}

impl<T> Sink for LibvirtTransport<T> where
    T: AsyncRead + AsyncWrite + 'static,
 {
    type SinkItem = (RequestId, LibvirtRequest);
    type SinkError = ::std::io::Error;

    fn start_send(&mut self, mut item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        use ::std::mem;

        if let Some(stream) = mem::replace(&mut item.1.stream, None) {
            debug!("SENDING REQ ID = {} {:?} WITH STREAM", item.0, item.1.header);
            let mut streams = self.streams.lock().unwrap();
            streams.insert(item.0, stream);
        }

        if let Some(sink) = mem::replace(&mut item.1.sink, None) {
            debug!("SENDING REQ ID = {} {:?} WITH SINK", item.0, item.1.header);
            {
                let mut sinks = self.sinks.lock().unwrap();
                sinks.insert(item.0, (sink, item.1.header.proc_));
            }
            self.process_sinks();
        }
        self.inner.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.close()
    }
}

#[derive(Debug, Clone)]
pub struct LibvirtProto {
    events: Arc<Mutex<HashMap<i32, ::futures::sync::mpsc::Sender<::request::DomainEvent>>>>,
}

impl LibvirtProto {
    pub fn new(
        events: Arc<Mutex<HashMap<i32, ::futures::sync::mpsc::Sender<::request::DomainEvent>>>>
    ) -> Self {
        LibvirtProto { events }
    }
}

impl<T> multiplex::ClientProto<T> for LibvirtProto where T: AsyncRead + AsyncWrite + 'static {
    type Request = LibvirtRequest;
    type Response = LibvirtResponse;
    type Transport = LibvirtTransport<T>;
    type BindTransport = Result<Self::Transport, ::std::io::Error>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        let framed = length_delimited::Builder::new()
                        .big_endian()
                        .length_field_offset(0)
                        .length_field_length(4)
                        .length_adjustment(-4)
                        .new_framed(io);
        Ok(LibvirtTransport{ 
            inner: framed_delimited(framed, LibvirtCodec),
            events: self.events.clone(),
            streams: Arc::new(Mutex::new(HashMap::new())),
            sinks: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

pub struct EventStream<T> {
    pub inner: ::futures::sync::mpsc::Receiver<T>,
}

impl<T> Stream for EventStream<T> {
    type Item = T;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

pub struct LibvirtStream<T> {
    pub inner: ::futures::sync::mpsc::Receiver<T>,
}

impl<T> Stream for LibvirtStream<T> {
    type Item = T;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

pub struct LibvirtSink {
    pub inner: ::futures::sync::mpsc::Sender<BytesMut>,
}

impl Sink for LibvirtSink {
    type SinkItem = BytesMut;
    type SinkError = ::futures::sync::mpsc::SendError<Self::SinkItem>;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.inner.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.close()
    }
}

impl Drop for LibvirtSink {
    fn drop(&mut self) {
        self.close();
        self.poll_complete();
    }
}