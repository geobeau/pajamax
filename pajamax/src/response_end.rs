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
    /// Successful reply: encode the message into the output buffer.
    Reply {
        stream_id: u32,
        encode_fn: Box<dyn FnOnce(&mut Vec<u8>)>,
    },
    /// Error status response.
    Status {
        stream_id: u32,
        status: crate::status::Status,
    },
    /// Window update after consuming DATA frames.
    WindowUpdate {
        len: usize,
    },
    /// Flush the output buffer now.
    Flush,
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
/// It drains the response channel and flushes batched output.
pub async fn writer_task(
    mut writer: OwnedWriteHalf<TcpStream>,
    mut resp_rx: RespRx,
    config: Config,
) -> Result<(), crate::error::Error> {
    let mut hpack_encoder = Encoder::new();
    let mut output: Vec<u8> = Vec::with_capacity(config.max_flush_size);
    let mut req_count: usize = 0;

    while let Some(job) = resp_rx.recv().await {
        let should_flush = matches!(&job, RespJob::Flush | RespJob::WindowUpdate { .. });

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
                req_count += 1;
            }
            RespJob::Status { stream_id, status } => {
                http2::build_status(stream_id, status, &mut hpack_encoder, &mut output);
                req_count += 1;
            }
            RespJob::WindowUpdate { len } => {
                if len > 0 {
                    http2::build_window_update(len, &mut output);
                }
            }
            RespJob::Flush => {}
        }

        if req_count >= config.max_flush_requests
            || output.len() >= config.max_flush_size
            || should_flush
        {
            if !output.is_empty() {
                trace!("flush response count:{} len:{}", req_count, output.len());
                let BufResult(res, buf) = writer.write_all(output).await;
                res?;
                output = buf;
                output.clear();
                req_count = 0;
            }
        }
    }

    // flush remaining
    if !output.is_empty() {
        let BufResult(res, _) = writer.write_all(output).await;
        res?;
    }

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
