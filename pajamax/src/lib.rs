//! Super fast gRPC server framework built on compio.
//!
//! Pajamax is a high-performance gRPC server framework that uses compio's
//! completion-based async runtime with a thread-per-core model for maximum
//! throughput.
//!
//! # Architecture
//!
//! Pajamax uses compio's thread-per-core model where each core runs its own
//! event loop with io_uring/iocp/polling. Each accepted TCP connection is
//! handled by an async task on the core that accepted it. This avoids
//! cross-thread synchronization and maximizes cache locality.
//!
//! # Optimization: Deep into HTTP/2
//!
//! gRPC runs over HTTP/2. gRPC and HTTP/2 are independent layers, and they SHOULD
//! also be independent in implementation. However, this independence also leads
//! to performance waste, mainly in the processing of request headers.
//!
//! - Typically, a standard HTTP/2 implementation must parse all request headers
//!   and return them to the upper-level application. But in a gRPC service, at
//!   least in specific scenarios, only the `:path` header is needed, while other
//!   headers can be ignored.
//!
//! - Even for the `:path` header, due to HPACK encoding, it needs allocate memory
//!   for an owned `String` before returning to the upper level to process. But
//!   in the specific scenario of gRPC, we can process directly on
//!   parsing `:path` in HTTP/2, thereby avoiding the memory allocation.
//!
//! # Modes: Local and Dispatch
//!
//! ## Local Mode
//!
//! The handler runs inline in the connection task. Simple and fast.
//!
//! ## Dispatch Mode
//!
//! Requests are dispatched to worker tasks via async channels on the same
//! core. Useful when you need to shard mutable state across workers.
//!
//! # Usage
//!
//! The usage of Pajamax is very similar to that of Tonic.
//!
//! See [`pajamax-build`](https://docs.rs/pajamax-build) crate document for more detail.

mod config;
pub mod connection;
mod hpack_decoder;
mod hpack_encoder;
mod http2;
mod huffman;
mod macros;

#[doc(hidden)]
pub mod dispatch;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod response_end;

pub mod status;
pub use config::{Config, ConfigedServer};

#[doc(hidden)]
pub use response_end::{send_response, RespTx};
#[doc(hidden)]
pub use http2::ReplyEncode;

/// Wrapper of `Result<Reply, status::Status>`.
pub type Response<Reply> = Result<Reply, status::Status>;

#[doc(hidden)]
/// Used by pajamax-build crate.
#[async_trait::async_trait(?Send)]
pub trait PajamaxService {
    // Route the path to request enum discriminant as usize.
    fn route(&self, path: &[u8]) -> Option<usize>;

    // Handle the request:
    // 1. parse the request from req_disc(from route()) and req_buf,
    // 2. call the method defined in applications and make reply,
    // 3. response the reply to resp_tx.
    async fn handle(&self, req_disc: usize, req_buf: &[u8], stream_id: u32, resp_tx: &response_end::RespTx) -> Result<(), error::Error>;

    fn is_dispatch_mode(&self) -> bool;
}
