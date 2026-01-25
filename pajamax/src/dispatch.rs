use std::cell::RefCell;
use std::net::TcpStream;
use std::sync::{mpsc, Arc, Mutex};

use crate::config::Config;
use crate::connection::local_build_response;
use crate::error::Error;
use crate::macros::*;
use crate::response_end::ResponseEnd;
use crate::status::{Code, Status};
use crate::ReplyEncode;
use crate::Response;

/// Send end of request channel for dispatch mode.
pub type RequestTx<Req> = mpsc::SyncSender<DispatchRequest<Req>>;

/// Receive end of request channel for dispatch mode.
pub type RequestRx<Req> = mpsc::Receiver<DispatchRequest<Req>>;

/// Send end of response channel for dispatch mode.
type ResponseTx = mpsc::SyncSender<DispatchResponse>;

/// Receive end of response channel for dispatch mode.
type ResponseRx = mpsc::Receiver<DispatchResponse>;

/// Dispatched request in dispatch mode.
pub struct DispatchRequest<Req> {
    pub stream: DispatchStream,
    pub request: Req,
}

/// Dispatched response in dispatch mode.
pub struct DispatchResponse {
    stream_id: u32,

    // We use dynamic-dispatch `dyn` here to accept different
    // response from multiple services in one channel.
    response: Response<Box<dyn ReplyEncode>>,
}

/// Stream context.
///
/// If your handler does not response immediately, you should return the
/// `status::Code::DispatchPending`, then the `{XX}ShardServer::handle()`
/// will return this `Stream`. You can use it later to response
/// when you are ready.
pub struct DispatchStream {
    stream_id: u32,
    resp_tx: ResponseTx,
}

impl DispatchStream {
    fn new(stream_id: u32) -> Self {
        Self {
            stream_id,
            resp_tx: RESP_TX.with_borrow(|tx| tx.clone()),
        }
    }
    pub fn response(self, response: Response<Box<dyn ReplyEncode>>) {
        let disp_resp = DispatchResponse {
            stream_id: self.stream_id,
            response,
        };
        let _ = self.resp_tx.send(disp_resp);
    }
}

thread_local! {
    static RESP_TX: RefCell<ResponseTx> = panic!();
}

// create a backend thread with response-channels
pub fn new_response_routine(c: Arc<Mutex<TcpStream>>, config: &Config) {
    let resp_end = ResponseEnd::new(c, config);

    let (resp_tx, resp_rx) = mpsc::sync_channel(config.max_concurrent_streams);

    RESP_TX.set(resp_tx);

    std::thread::Builder::new()
        .name(String::from("pajamax-r")) // response routine
        .spawn(move || response_routine(resp_end, resp_rx))
        .unwrap();
}

// dispatch the request to req_tx
pub fn dispatch<Req>(req_tx: &RequestTx<Req>, request: Req, stream_id: u32) -> Result<(), Error> {
    trace!("dispatch request id:{stream_id}");

    let stream = DispatchStream::new(stream_id);
    let disp_req = DispatchRequest { stream, request };

    match req_tx.try_send(disp_req) {
        Ok(_) => Ok(()),
        Err(err) => {
            error!("dispatch fails (stream_id:{stream_id}): {:?}", err);
            let status = match err {
                mpsc::TrySendError::Full(_) => Status {
                    code: Code::Unavailable,
                    message: String::from("dispatch channel is full"),
                },
                mpsc::TrySendError::Disconnected(_) => Status {
                    code: Code::Internal,
                    message: String::from("dispatch channel is closed"),
                },
            };
            let response: Response<()> = Err(status);
            local_build_response(stream_id, response)
        }
    }
}

// output thread
fn response_routine(mut resp_end: ResponseEnd, resp_rx: ResponseRx) -> Result<(), Error> {
    loop {
        let resp = response_receive(&mut resp_end, &resp_rx)?;
        trace!("receive dispatched response {}", resp.stream_id);
        resp_end.build_box(resp.stream_id, resp.response)?;
    }
}

fn response_receive(
    resp_end: &mut ResponseEnd,
    resp_rx: &ResponseRx,
) -> Result<DispatchResponse, Error> {
    // The blocking-mode 'recv()' will register at the channel, and
    // the sender end (in application business thread) will wake up it
    // later by syscall, which is expensive. In order to reduce the
    // performance consumption of business thread, we first poll in
    // non-blocking mode for several times.

    // poll in non-blocking mode, at most 500ms
    for i in 0..1000 {
        match resp_rx.try_recv() {
            Ok(resp) => {
                return Ok(resp);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(Error::ChannelClosed);
            }
            Err(mpsc::TryRecvError::Empty) => {
                resp_end.flush()?;
                std::thread::sleep(std::time::Duration::from_micros(i));
            }
        }
    }

    // wait in blocking mode
    Ok(resp_rx.recv()?)
}

pub fn pending<T>() -> Response<T> {
    Err(Status {
        code: Code::DispatchPending,
        message: String::new(),
    })
}

pub fn is_pending(resp: &Response<Box<dyn ReplyEncode>>) -> bool {
    if let Err(status) = resp {
        status.code == Code::DispatchPending
    } else {
        false
    }
}
