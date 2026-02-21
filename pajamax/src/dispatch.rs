use crate::error::Error;
use crate::macros::*;
use crate::response_end::{self, RespTx};
use crate::status::{Code, Status};
use crate::ReplyEncode;
use crate::Response;

/// Send end of request channel for dispatch mode.
/// Uses unbounded channel so dispatch is non-blocking.
pub type RequestTx<Req> = local_sync::mpsc::unbounded::Tx<DispatchRequest<Req>>;

/// Receive end of request channel for dispatch mode.
pub type RequestRx<Req> = local_sync::mpsc::unbounded::Rx<DispatchRequest<Req>>;

/// Dispatched request in dispatch mode.
pub struct DispatchRequest<Req> {
    pub stream: DispatchStream,
    pub request: Req,
}

/// Stream context for dispatch mode.
pub struct DispatchStream {
    stream_id: u32,
    resp_tx: RespTx,
}

impl DispatchStream {
    pub(crate) fn new(stream_id: u32, resp_tx: RespTx) -> Self {
        Self { stream_id, resp_tx }
    }

    pub fn response(self, response: Response<Box<dyn ReplyEncode>>) {
        let _ = response_end::send_response_box(&self.resp_tx, self.stream_id, response);
    }
}

/// Dispatch a request to the given channel.
pub fn dispatch<Req>(
    req_tx: &RequestTx<Req>,
    request: Req,
    stream_id: u32,
    resp_tx: &RespTx,
) -> Result<(), Error> {
    trace!("dispatch request id:{stream_id}");

    let stream = DispatchStream::new(stream_id, resp_tx.clone());
    let disp_req = DispatchRequest { stream, request };

    match req_tx.send(disp_req) {
        Ok(_) => Ok(()),
        Err(_) => {
            error!("dispatch fails (stream_id:{stream_id})");
            let status = Status {
                code: Code::Unavailable,
                message: String::from("dispatch channel is closed"),
            };
            let response: Response<Box<dyn ReplyEncode>> = Err(status);
            response_end::send_response_box(resp_tx, stream_id, response)
        }
    }
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
