use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use compio::io::AsyncReadManaged;
use compio::net::{TcpListener, TcpStream};
use futures_util::StreamExt;

use crate::config::Config;
use crate::error::Error;
use crate::hpack_decoder::{Decoder, GrpcEncoding, PathKind};
use crate::http2::*;
use crate::macros::*;
use crate::response_end;
use crate::PajamaxService;

/// Balances accepted connections across worker loops using round-robin.
pub struct ConnectionBalancer {
    workers: std::sync::Mutex<Vec<flume::Sender<std::net::TcpStream>>>,
    counter: AtomicUsize,
}

impl ConnectionBalancer {
    pub fn new() -> Self {
        Self {
            workers: std::sync::Mutex::new(Vec::new()),
            counter: AtomicUsize::new(0),
        }
    }

    /// Register a new worker and return its async receiver channel.
    pub fn register(&self) -> flume::Receiver<std::net::TcpStream> {
        let (tx, rx) = flume::unbounded();
        self.workers.lock().unwrap().push(tx);
        rx
    }

    /// Dispatch a connection to the next worker (round-robin).
    pub fn dispatch(&self, stream: std::net::TcpStream) {
        let workers = self.workers.lock().unwrap();
        let idx = self.counter.fetch_add(1, Ordering::Relaxed) % workers.len();
        let _ = workers[idx].send(stream);
    }
}

/// Server configuration. Created once, shared across all worker threads.
///
/// Use [`serve`] to start accepting and handling connections with this
/// configuration. The [`ConnectionBalancer`] inside distributes accepted
/// connections across all registered runtimes via round-robin.
#[derive(Clone)]
pub struct Server {
    config: Config,
    addr: String,
    factories: std::sync::Arc<Vec<Box<dyn Fn() -> Rc<dyn PajamaxService> + Send + Sync>>>,
    balancer: std::sync::Arc<ConnectionBalancer>,
}

impl Server {
    pub fn new(
        service_factories: Vec<Box<dyn Fn() -> Rc<dyn PajamaxService> + Send + Sync>>,
        config: Config,
        addr: String,
    ) -> Self {
        Self {
            config,
            addr,
            factories: std::sync::Arc::new(service_factories),
            balancer: std::sync::Arc::new(ConnectionBalancer::new()),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
}

/// Start accepting and handling connections on the current compio runtime.
///
/// Must be called from within a compio runtime. Each call registers with the
/// shared balancer so connections are distributed across all runtimes.
pub async fn serve(server: Server) -> std::io::Result<()> {
    let services: Vec<Rc<dyn PajamaxService>> =
        server.factories.iter().map(|f| f()).collect();
    let rx = server.balancer.register();
    balanced_loop(services, server.config, server.addr.clone(), server.balancer.clone(), rx).await
}

pub async fn accept_loop(
    services: Vec<Rc<dyn PajamaxService>>,
    config: Config,
    addr: String,
) -> std::io::Result<()> {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    socket.set_reuse_port(true)?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.parse::<std::net::SocketAddr>().unwrap().into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std::net::TcpListener::from(socket))?;
    info!("listening on {}", addr);

    let concurrent = Rc::new(std::cell::Cell::new(0usize));
    let mut connections = 0;
    loop {
        let (stream, peer) = listener.accept().await?;
        stream.set_nodelay(true)?;

        // concurrent limit
        if concurrent.get() >= config.max_concurrent_connections {
            error!("drop new connection for limit");
            continue;
        }
        concurrent.set(concurrent.get() + 1);

        info!("new connection from {:?}", peer);

        let services = services.clone();
        let concurrent = concurrent.clone();
        connections += 1;
        println!("Handled {connections} connections");
        compio::runtime::spawn(async move {
            match handle_connection(services, stream, config).await {
                Ok(_) => println!("connection closed"),
                Err(err) => println!("connection fail: {:?}", err),
            }
            concurrent.set(concurrent.get() - 1);
        }).detach();
    }
}

/// Combined accept + worker loop for balanced mode.
/// Each thread accepts connections via SO_REUSEPORT and dispatches them
/// round-robin through the balancer. A separate worker loop receives
/// connections from the balancer channel and handles them.
async fn balanced_loop(
    services: Vec<Rc<dyn PajamaxService>>,
    config: Config,
    addr: String,
    balancer: std::sync::Arc<ConnectionBalancer>,
    rx: flume::Receiver<std::net::TcpStream>,
) -> std::io::Result<()> {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    socket.set_reuse_port(true)?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.parse::<std::net::SocketAddr>().unwrap().into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std::net::TcpListener::from(socket))?;
    info!("listening on {}", addr);

    // Accept task: AcceptMulti drains a burst of accepts per kernel wakeup and
    // dispatches each to the balancer round-robin. The balancer hop is preserved.
    compio::runtime::spawn(async move {
        let mut incoming = listener.incoming();
        while let Some(res) = incoming.next().await {
            match res {
                Ok(stream) => {
                    let owned_fd = match std::os::fd::AsFd::as_fd(&stream).try_clone_to_owned() {
                        Ok(fd) => fd,
                        Err(e) => {
                            error!("failed to dup accepted fd: {:?}", e);
                            continue;
                        }
                    };
                    drop(stream);
                    let std_stream = std::net::TcpStream::from(owned_fd);
                    balancer.dispatch(std_stream);
                }
                Err(e) => {
                    error!("accept error: {:?}", e);
                }
            }
        }
    }).detach();

    // Worker loop: receive connections from balancer channel via flume async.

    let concurrent = Rc::new(std::cell::Cell::new(0usize));
    let mut connections = 0;
    while let Ok(std_stream) = rx.recv_async().await {
        std_stream.set_nodelay(true).ok();

        if concurrent.get() >= config.max_concurrent_connections {
            error!("drop new connection for limit");
            continue;
        }
        concurrent.set(concurrent.get() + 1);

        let stream = TcpStream::from_std(std_stream)?;
        info!("new connection (balanced)");

        let services = services.clone();
        let concurrent = concurrent.clone();
        connections += 1;
        println!("Handled {connections} connections");
        compio::runtime::spawn(async move {
            match handle_connection(services, stream, config).await {
                Ok(_) => println!("connection closed"),
                Err(err) => println!("connection fail: {:?}", err),
            }
            concurrent.set(concurrent.get() - 1);
        }).detach();
    }

    Ok(())
}

struct Stream {
    id: u32,
    isvc: usize,
    req_disc: usize,
    encoding: GrpcEncoding,
    data: Vec<u8>,
}

async fn handle_connection(
    services: Vec<Rc<dyn PajamaxService>>,
    stream: TcpStream,
    config: Config,
) -> Result<(), Error> {
    // Handshake on full stream before splitting
    let mut stream = stream;
    crate::http2::handshake(&mut stream, &config).await?;
    trace!("handshake done");

    // Clone the stream for the writer (cheap fd dup); keep original for read_managed
    let writer = stream.clone();

    // Create response channel
    let (resp_tx, resp_rx) = response_end::resp_channel();

    // Spawn writer task
    compio::runtime::spawn(async move {
        if let Err(e) = response_end::writer_task(writer, resp_rx, config).await {
            error!("writer task error: {:?}", e);
        }
    }).detach();

    // Streaming frame parser state
    let mut streams: VecDeque<Stream> = VecDeque::new();
    let mut hpack_decoder = Decoder::new();
    let mut route_cache: Vec<(usize, usize)> = Vec::new();
    let mut continuation_buf: Option<(u32, bool, Vec<u8>)> = None;
    let mut last_client_stream_id: u32 = 0;

    // State machine buffers
    let mut header_buf = [0u8; 9];
    let mut header_len: usize = 0;
    let mut control_buf: Vec<u8> = Vec::new();
    let mut cur_kind = FrameKind::Unknown;
    let mut cur_flags = HeadFlags::from(0);
    let mut cur_stream_id: u32 = 0;
    let mut cur_payload_len: usize = 0;
    let mut cur_payload_read: usize = 0;
    let mut state = ParseState::Header;

    // Window update tracking across a full read
    let mut data_len: usize = 0;
    let mut stream_data_lens: Vec<(u32, usize)> = Vec::new();

    loop {
        let managed_buf = match stream.read_managed(0).await {
            Ok(None) => return Ok(()), // EOF / connection closed
            Ok(Some(buf)) if buf.is_empty() => return Ok(()),
            Ok(Some(buf)) => buf,
            Err(e) => return Err(Error::IoFail(e)),
        };
        let len = managed_buf.len();
        let read_buf: &[u8] = &managed_buf;

        trace!("receive data {len}");

        data_len = 0;
        stream_data_lens.clear();

        let mut pos = 0;
        while pos < len {
            match state {
                ParseState::Header => {
                    let need = 9 - header_len;
                    let avail = len - pos;
                    let take = need.min(avail);
                    header_buf[header_len..header_len + take].copy_from_slice(&read_buf[pos..pos + take]);
                    header_len += take;
                    pos += take;

                    if header_len < 9 {
                        continue;
                    }

                    let hdr = FrameHeader::parse(&header_buf).unwrap();
                    cur_kind = hdr.kind;
                    cur_flags = hdr.flags;
                    cur_stream_id = hdr.stream_id;
                    cur_payload_len = hdr.len;
                    cur_payload_read = 0;
                    header_len = 0;

                    trace!(
                        "get frame {:?} {:?}, len:{}, stream_id:{}",
                        cur_kind, cur_flags, cur_payload_len, cur_stream_id
                    );

                    if cur_payload_len == 0 {
                        if process_frame(
                            cur_kind, cur_flags, cur_stream_id, cur_payload_len,
                            &[], &services, &resp_tx,
                            &mut streams, &mut hpack_decoder, &mut route_cache,
                            &mut continuation_buf, &mut last_client_stream_id,
                            &mut data_len, &mut stream_data_lens, &config,
                        )? {
                            return Ok(());
                        }
                    } else {
                        if cur_kind == FrameKind::Data && !cur_flags.is_padded() {
                            if let Some(s) = streams.iter_mut().find(|s| s.id == cur_stream_id) {
                                s.data.reserve(cur_payload_len);
                            }
                        } else {
                            control_buf.clear();
                            control_buf.reserve(cur_payload_len);
                        }
                        state = ParseState::Payload;
                    }
                }
                ParseState::Payload => {
                    let remaining = cur_payload_len - cur_payload_read;
                    let avail = len - pos;
                    let take = remaining.min(avail);

                    if cur_kind == FrameKind::Data && !cur_flags.is_padded() {
                        if let Some(s) = streams.iter_mut().find(|s| s.id == cur_stream_id) {
                            s.data.extend_from_slice(&read_buf[pos..pos + take]);
                        }
                    } else {
                        control_buf.extend_from_slice(&read_buf[pos..pos + take]);
                    }

                    cur_payload_read += take;
                    pos += take;

                    if cur_payload_read < cur_payload_len {
                        continue;
                    }

                    // Frame complete
                    if cur_kind == FrameKind::Data && !cur_flags.is_padded() {
                        data_len += cur_payload_len;
                        stream_data_lens.push((cur_stream_id, cur_payload_len));

                        let Some(i) = streams.iter().position(|s| s.id == cur_stream_id) else {
                            state = ParseState::Header;
                            continue;
                        };

                        if !cur_flags.is_end_stream() {
                            state = ParseState::Header;
                            continue;
                        }

                        let Stream { id, isvc, req_disc, encoding, data, .. } = streams.remove(i).unwrap();

                        let data = decode_grpc_message(data, encoding)?;

                        trace!("handle isvc:{isvc}, req_disc:{req_disc}");

                        let svc = services[isvc].clone();
                        let resp_tx = resp_tx.clone();
                        compio::runtime::spawn(async move {
                            if let Err(e) = svc.handle(req_disc, data, id, &resp_tx).await {
                                if !matches!(e, Error::ChannelClosed) {
                                    error!("handle error: {:?}", e);
                                }
                            }
                        }).detach();
                    } else {
                        if process_frame(
                            cur_kind, cur_flags, cur_stream_id, cur_payload_len,
                            &control_buf, &services, &resp_tx,
                            &mut streams, &mut hpack_decoder, &mut route_cache,
                            &mut continuation_buf, &mut last_client_stream_id,
                            &mut data_len, &mut stream_data_lens, &config,
                        )? {
                            return Ok(());
                        }
                    }
                    state = ParseState::Header;
                }
            }
        }

        // Send window update
        if data_len > 0 {
            let _ = resp_tx.send(response_end::RespJob::WindowUpdate { len: data_len, stream_data_lens: std::mem::take(&mut stream_data_lens) });
        }
    }
}

enum ParseState {
    Header,
    Payload,
}

/// Process a complete frame (or zero-length frame). Returns true if the
/// connection should be closed (GoAway).
fn process_frame(
    kind: FrameKind,
    flags: HeadFlags,
    stream_id: u32,
    payload_len: usize,
    payload: &[u8],
    services: &[Rc<dyn PajamaxService>],
    resp_tx: &response_end::RespTx,
    streams: &mut VecDeque<Stream>,
    hpack_decoder: &mut Decoder,
    route_cache: &mut Vec<(usize, usize)>,
    continuation_buf: &mut Option<(u32, bool, Vec<u8>)>,
    last_client_stream_id: &mut u32,
    data_len: &mut usize,
    stream_data_lens: &mut Vec<(u32, usize)>,
    config: &Config,
) -> Result<bool, Error> {
    // When expecting CONTINUATION, reject any other frame type
    if let Some((cont_stream_id, _, _)) = continuation_buf {
        if kind != FrameKind::Continuation {
            return Err(Error::InvalidHttp2("expected CONTINUATION frame"));
        }
        if stream_id != *cont_stream_id {
            return Err(Error::InvalidHttp2("CONTINUATION stream ID mismatch"));
        }
    }

    match kind {
        FrameKind::Data | FrameKind::Headers if stream_id == 0 => {
            return Err(Error::InvalidHttp2("DATA/HEADERS must not be on stream 0"));
        }
        FrameKind::Settings | FrameKind::Ping | FrameKind::GoAway if stream_id != 0 => {
            return Err(Error::InvalidHttp2("SETTINGS/PING/GOAWAY must be on stream 0"));
        }
        FrameKind::Data => {
            let req_buf = Frame::strip_data_padding(flags, payload)?;

            *data_len += payload_len;
            stream_data_lens.push((stream_id, payload_len));

            let Some(i) = streams.iter().position(|s| s.id == stream_id) else {
                return Ok(false);
            };

            streams[i].data.extend_from_slice(req_buf);

            if !flags.is_end_stream() {
                return Ok(false);
            }

            let Stream { id, isvc, req_disc, encoding, data, .. } = streams.remove(i).unwrap();

            let data = decode_grpc_message(data, encoding)?;

            trace!("handle isvc:{isvc}, req_disc:{req_disc}");

            let svc = services[isvc].clone();
            let resp_tx = resp_tx.clone();
            compio::runtime::spawn(async move {
                if let Err(e) = svc.handle(req_disc, data, id, &resp_tx).await {
                    if !matches!(e, Error::ChannelClosed) {
                        error!("handle error: {:?}", e);
                    }
                }
            }).detach();
        }
        FrameKind::Headers => {
            if stream_id % 2 == 0 {
                return Err(Error::InvalidHttp2("client stream ID must be odd"));
            }
            if stream_id <= *last_client_stream_id {
                return Err(Error::InvalidHttp2("client stream ID must be monotonically increasing"));
            }
            *last_client_stream_id = stream_id;

            if streams.len() >= config.max_concurrent_streams {
                let mut output = Vec::new();
                crate::http2::build_rst_stream(stream_id, 7, &mut output);
                let _ = resp_tx.send(response_end::RespJob::Write { buf: output });
                return Ok(false);
            }

            let headers_buf = Frame::strip_headers(flags, payload)?;

            if !flags.is_end_headers() {
                let buf = Vec::from(headers_buf);
                *continuation_buf = Some((stream_id, flags.is_end_stream(), buf));
                return Ok(false);
            }

            let end_stream = flags.is_end_stream();
            let (isvc, req_disc, encoding) = resolve_route(hpack_decoder, route_cache, services, headers_buf)?;

            if end_stream {
                let svc = services[isvc].clone();
                let resp_tx = resp_tx.clone();
                trace!("handle (no body) isvc:{isvc}, req_disc:{req_disc}");
                compio::runtime::spawn(async move {
                    if let Err(e) = svc.handle(req_disc, bytes::Bytes::new(), stream_id, &resp_tx).await {
                        if !matches!(e, Error::ChannelClosed) {
                            error!("handle error: {:?}", e);
                        }
                    }
                }).detach();
            } else {
                streams.push_back(Stream {
                    id: stream_id,
                    isvc,
                    req_disc,
                    encoding,
                    data: Vec::new(),
                });
            }
        }
        FrameKind::Continuation => {
            let Some((cont_stream_id, end_stream, ref mut buf)) = continuation_buf else {
                return Err(Error::InvalidHttp2("unexpected CONTINUATION frame"));
            };

            buf.extend_from_slice(payload);

            if !flags.is_end_headers() {
                return Ok(false);
            }

            let stream_id = *cont_stream_id;
            let end_stream = *end_stream;
            let headers_buf = std::mem::take(buf);
            *continuation_buf = None;

            let (isvc, req_disc, encoding) = resolve_route(hpack_decoder, route_cache, services, &headers_buf)?;

            if end_stream {
                let svc = services[isvc].clone();
                let resp_tx = resp_tx.clone();
                trace!("handle (no body) isvc:{isvc}, req_disc:{req_disc}");
                compio::runtime::spawn(async move {
                    if let Err(e) = svc.handle(req_disc, bytes::Bytes::new(), stream_id, &resp_tx).await {
                        if !matches!(e, Error::ChannelClosed) {
                            error!("handle error: {:?}", e);
                        }
                    }
                }).detach();
            } else {
                streams.push_back(Stream {
                    id: stream_id,
                    isvc,
                    req_disc,
                    encoding,
                    data: Vec::new(),
                });
            }
        }
        FrameKind::Settings => {
            if !flags.is_ack() {
                let mut offset = 0;
                while offset + 6 <= payload_len {
                    let ident = u16::from_be_bytes([payload[offset], payload[offset + 1]]);
                    let value = u32::from_be_bytes([
                        payload[offset + 2], payload[offset + 3],
                        payload[offset + 4], payload[offset + 5],
                    ]);
                    match ident {
                        1 => trace!("client SETTINGS_HEADER_TABLE_SIZE: {value}"),
                        2 => trace!("client SETTINGS_ENABLE_PUSH: {value}"),
                        3 => trace!("client SETTINGS_MAX_CONCURRENT_STREAMS: {value}"),
                        4 => trace!("client SETTINGS_INITIAL_WINDOW_SIZE: {value}"),
                        5 => trace!("client SETTINGS_MAX_FRAME_SIZE: {value}"),
                        6 => trace!("client SETTINGS_MAX_HEADER_LIST_SIZE: {value}"),
                        _ => trace!("client SETTINGS unknown ident:{ident} value:{value}"),
                    }
                    offset += 6;
                }

                let mut output = Vec::new();
                crate::http2::build_settings_ack(&mut output);
                let _ = resp_tx.send(response_end::RespJob::Write { buf: output });
            }
        }
        FrameKind::Ping => {
            if !flags.is_ping_ack() {
                let mut output = Vec::new();
                crate::http2::build_ping_ack(payload, &mut output);
                let _ = resp_tx.send(response_end::RespJob::Write { buf: output });
            }
        }
        FrameKind::GoAway => {
            if payload_len < 8 {
                return Err(Error::InvalidHttp2("GOAWAY payload must be at least 8 bytes"));
            }
            let last_stream_id = u32::from_be_bytes([
                payload[0] & 0x7f, payload[1], payload[2], payload[3],
            ]);
            let error_code = u32::from_be_bytes([
                payload[4], payload[5], payload[6], payload[7],
            ]);
            trace!("GOAWAY last_stream_id:{last_stream_id} error_code:{error_code}");
            return Ok(true);
        }
        FrameKind::Reset => {
            if payload_len != 4 {
                return Err(Error::InvalidHttp2("RST_STREAM payload must be 4 bytes"));
            }
            if let Some(i) = streams.iter().position(|s| s.id == stream_id) {
                streams.remove(i);
            }
            trace!("RST_STREAM stream_id:{}", stream_id);
        }
        FrameKind::WindowUpdate => {
            if payload_len != 4 {
                return Err(Error::InvalidHttp2("WINDOW_UPDATE payload must be 4 bytes"));
            }
            let increment = u32::from_be_bytes([
                payload[0] & 0x7f, payload[1], payload[2], payload[3],
            ]);
            if increment == 0 {
                return Err(Error::InvalidHttp2("WINDOW_UPDATE increment must be non-zero"));
            }
            trace!("WINDOW_UPDATE stream_id:{stream_id} increment:{increment}");
        }
        _ => (),
    }
    Ok(false)
}

fn decode_grpc_message(data: Vec<u8>, encoding: GrpcEncoding) -> Result<bytes::Bytes, Error> {
    if data.len() < 5 {
        return Err(Error::InvalidHttp2("DATA too short for grpc"));
    }
    let compressed = data[0] != 0;

    if !compressed {
        return Ok(bytes::Bytes::from(data).slice(5..));
    }

    use std::io::Read;
    let mut decoded = Vec::new();
    match encoding {
        GrpcEncoding::Gzip => {
            flate2::read::GzDecoder::new(&data[5..])
                .read_to_end(&mut decoded)
                .map_err(Error::IoFail)?;
        }
        GrpcEncoding::Deflate => {
            flate2::read::DeflateDecoder::new(&data[5..])
                .read_to_end(&mut decoded)
                .map_err(Error::IoFail)?;
        }
        GrpcEncoding::Zstd => {
            zstd::Decoder::new(&data[5..])
                .map_err(Error::IoFail)?
                .read_to_end(&mut decoded)
                .map_err(Error::IoFail)?;
        }
        GrpcEncoding::Identity => {
            return Err(Error::InvalidHttp2("compressed flag set but no grpc-encoding"));
        }
    }
    Ok(bytes::Bytes::from(decoded))
}

fn resolve_route(
    hpack_decoder: &mut Decoder,
    route_cache: &mut Vec<(usize, usize)>,
    services: &[Rc<dyn PajamaxService>],
    headers_buf: &[u8],
) -> Result<(usize, usize, GrpcEncoding), Error> {
    let result = hpack_decoder.find_path_and_encoding(headers_buf)?;
    let (isvc, req_disc) = match result.path {
        PathKind::Cached(cached) => {
            trace!("route cache hit: {cached}");
            route_cache[cached]
        }
        PathKind::Plain(path) => {
            let len0 = route_cache.len();
            for (i, svc) in services.iter().enumerate() {
                if let Some(req_disc) = svc.route(&path) {
                    route_cache.push((i, req_disc));
                    break;
                }
            }
            if route_cache.len() == len0 {
                return Err(Error::UnknownMethod(
                    String::from_utf8_lossy(&path).into(),
                ));
            }
            trace!(
                "route cache new ({len0}): {}",
                String::from_utf8_lossy(&path)
            );
            route_cache[len0]
        }
    };
    Ok((isvc, req_disc, result.encoding))
}
