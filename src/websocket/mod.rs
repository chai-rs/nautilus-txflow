//! TxFlow WebSocket transport and frames.
//!
//! - [`client`]: connection, ping, reconnect with backoff, subscription replay. One type serves
//!   both endpoints (`/ws` and `/api-ws`).
//! - [`messages`]: the frames the venue sends.
//! - [`error`]: the client error type.

pub mod client;
pub mod error;
pub mod messages;

pub use client::TxflowWebSocketClient;
pub use error::TxflowWsError;
pub use messages::WsMessage;
