use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use compio::io::AsyncRead;
use compio::net::{TcpListener, TcpStream};
use compio::BufResult;

use crate::config::Config;
use crate::error::Error;
use crate::hpack_decoder::{Decoder, PathKind};
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

    // Accept task: accepts connections and dispatches to balancer round-robin
    compio::runtime::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
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

    // Worker loop: receive connections from balancer channel via flume async
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

    // Split into read and write halves
    let (mut reader, writer) = stream.into_split();

    // Create response channel
    let (resp_tx, resp_rx) = response_end::resp_channel();

    // Spawn writer task
    compio::runtime::spawn(async move {
        if let Err(e) = response_end::writer_task(writer, resp_rx, config).await {
            error!("writer task error: {:?}", e);
        }
    }).detach();

    // Read and parse input data
    let mut input: Vec<u8> = vec![0u8; config.max_frame_size];
    let mut last_end: usize = 0;
    let mut streams: VecDeque<Stream> = VecDeque::new();
    let mut hpack_decoder = Decoder::new();
    let mut route_cache: Vec<(usize, usize)> = Vec::new();

    loop {
        // Read data using ownership-based I/O
        let read_buf = if last_end < input.len() {
            // We have space in the buffer after last_end
            let slice = input.split_off(last_end);
            let remaining = input;
            input = remaining;
            slice
        } else {
            vec![0u8; config.max_frame_size]
        };

        let BufResult(res, returned_buf) = reader.read(read_buf).await;
        let len = match res {
            Ok(0) => return Ok(()), // connection closed
            Ok(n) => n,
            Err(e) => return Err(Error::IoFail(e)),
        };

        trace!("receive data {len}");

        // Append read data to input buffer
        input.extend_from_slice(&returned_buf[..len]);
        let end = input.len();

        let mut data_len = 0;
        let mut stream_data_lens: Vec<(u32, usize)> = Vec::new();
        let mut pos = 0;
        while let Some(frame) = Frame::parse(&input[pos..end]) {
            pos += Frame::HEAD_SIZE + frame.len;

            trace!(
                "get frame {:?} {:?}, len:{}, stream_id:{}",
                frame.kind,
                frame.flags,
                frame.len,
                frame.stream_id
            );

            match frame.kind {
                FrameKind::Headers => {
                    let headers_buf = frame.process_headers()?;

                    let (isvc, req_disc) = match hpack_decoder.find_path(headers_buf)? {
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

                    streams.push_back(Stream {
                        id: frame.stream_id,
                        isvc,
                        req_disc,
                        data: Vec::new(),
                    });
                }

                FrameKind::Settings => {
                    if !frame.flags.is_ack() {
                        let mut output = Vec::new();
                        crate::http2::build_settings_ack(&mut output);
                        let _ = resp_tx.send(response_end::RespJob::Write { buf: output });
                    }
                }
                FrameKind::Ping => {
                    if !frame.flags.is_ping_ack() {
                        let mut output = Vec::new();
                        crate::http2::build_ping_ack(frame.payload, &mut output);
                        let _ = resp_tx.send(response_end::RespJob::Write { buf: output });
                    }
                }
                FrameKind::Data => {
                    let req_buf = frame.process_data()?;

                    if req_buf.len() == 0 {
                        continue;
                    }
                    if req_buf.len() < 5 {
                        return Err(Error::InvalidHttp2("DATA frame too short for grpc"));
                    }
                    let req_buf = &req_buf[5..];

                    let Some(i) = streams.iter().position(|s| s.id == frame.stream_id) else {
                        // Follow-up DATA frame for an already-dispatched stream; ignore
                        data_len += frame.len;
                        stream_data_lens.push((frame.stream_id, frame.len));
                        continue;
                    };
                    let Stream { id, isvc, req_disc, .. } = streams.remove(i).unwrap();

                    trace!("handle isvc:{isvc}, req_disc:{req_disc}");

                    let req_buf = req_buf.to_vec();
                    let svc = services[isvc].clone();
                    let resp_tx = resp_tx.clone();
                    compio::runtime::spawn(async move {
                        if let Err(e) = svc.handle(req_disc, &req_buf, id, &resp_tx).await {
                            error!("handle error: {:?}", e);
                        }
                    }).detach();

                    data_len += frame.len;
                    stream_data_lens.push((frame.stream_id, frame.len));
                }
                _ => (),
            }
        }

        // Send window update
        if data_len > 0 {
            let _ = resp_tx.send(response_end::RespJob::WindowUpdate { len: data_len, stream_data_lens });
        }

        // Shift leftover data for next loop
        if pos == 0 {
            return Err(Error::InvalidHttp2("too long frame"));
        }
        if pos < end {
            trace!("left data {}", end - pos);
            input.copy_within(pos..end, 0);
            input.truncate(end - pos);
            last_end = end - pos;
        } else {
            input.clear();
            last_end = 0;
        }
        // Ensure input has capacity for next read
        if input.len() < config.max_frame_size {
            input.resize(config.max_frame_size, 0);
        }
    }
}
