use compio::io::AsyncWriteExt;
use compio::net::{OwnedWriteHalf, TcpStream};
use compio::BufResult;

use crate::config::Config;
use crate::hpack_encoder::Encoder;
use crate::http2;
use crate::macros::*;
use crate::Response;

/// A response job sent from handlers to the writer task.
pub enum RespJob {
    Reply {
        stream_id: u32,
        encode_fn: Box<dyn FnOnce(&mut Vec<u8>)>,
    },
    Status {
        stream_id: u32,
        status: crate::status::Status,
    },
    WindowUpdate {
        len: usize,
        stream_data_lens: Vec<(u32, usize)>,
    },
    Write {
        buf: Vec<u8>,
    },
}

/// The response sender that handlers use to submit responses.
/// Uses unbounded channel so send is synchronous (non-async).
pub type RespTx = local_sync::mpsc::unbounded::Tx<RespJob>;
pub type RespRx = local_sync::mpsc::unbounded::Rx<RespJob>;

/// Create a response channel pair.
pub fn resp_channel() -> (RespTx, RespRx) {
    local_sync::mpsc::unbounded::channel()
}

/// The writer task that owns the write half of the TCP connection.
/// It writes each response to the socket immediately.
pub async fn writer_task(
    mut writer: OwnedWriteHalf<TcpStream>,
    mut resp_rx: RespRx,
    _config: Config,
) -> Result<(), crate::error::Error> {
    let mut hpack_encoder = Encoder::new();
    let mut output: Vec<u8> = Vec::with_capacity(4096);

    while let Some(job) = resp_rx.recv().await {
        match job {
            RespJob::Reply {
                stream_id,
                encode_fn,
            } => {
                http2::build_response(
                    stream_id,
                    encode_fn,
                    &mut hpack_encoder,
                    &mut output,
                );
            }
            RespJob::Status { stream_id, status } => {
                http2::build_status(stream_id, status, &mut hpack_encoder, &mut output);
            }
            RespJob::WindowUpdate { len, stream_data_lens } => {
                if len > 0 {
                    http2::build_window_update(len, 0, &mut output);
                    for (stream_id, stream_len) in stream_data_lens {
                        http2::build_window_update(stream_len, stream_id, &mut output);
                    }
                }
            }
            RespJob::Write { buf } => {
                output.extend(buf);
            }
        }

        if !output.is_empty() {
            trace!("write response len:{}", output.len());
            let BufResult(res, buf) = writer.write_all(output).await;
            if let Err(ref e) = res {
                println!("connection closed: write error: {:?}", e);
            }
            res?;
            output = buf;
            output.clear();
        }
    }

    println!("connection closed: response channel closed");
    Ok(())
}

/// Helper to build and send a successful response through the channel.
pub fn send_response<Reply>(
    resp_tx: &RespTx,
    stream_id: u32,
    response: Response<Reply>,
) -> Result<(), crate::error::Error>
where
    Reply: prost::Message + 'static,
{
    let job = match response {
        Ok(reply) => RespJob::Reply {
            stream_id,
            encode_fn: Box::new(move |output| reply.encode(output).unwrap()),
        },
        Err(status) => RespJob::Status { stream_id, status },
    };
    resp_tx
        .send(job)
        .map_err(|_| crate::error::Error::ChannelClosed)
}

/// Helper to build and send a boxed response (for dispatch mode).
pub fn send_response_box(
    resp_tx: &RespTx,
    stream_id: u32,
    response: Response<Box<dyn http2::ReplyEncode>>,
) -> Result<(), crate::error::Error> {
    let job = match response {
        Ok(reply) => RespJob::Reply {
            stream_id,
            encode_fn: Box::new(move |output| reply.encode(output).unwrap()),
        },
        Err(status) => RespJob::Status { stream_id, status },
    };
    resp_tx
        .send(job)
        .map_err(|_| crate::error::Error::ChannelClosed)
}
