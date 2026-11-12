//! Venue constants.

use std::sync::LazyLock;

use nautilus_model::identifiers::Venue;

pub const TXFLOW: &str = "TXFLOW";

pub static TXFLOW_VENUE: LazyLock<Venue> = LazyLock::new(|| Venue::new(TXFLOW));

/// Required on every `/info` and `/exchange` request.
pub const API_VERSION_HEADER: &str = "x-api-version";
pub const API_VERSION: &str = "2026-08-16";
/// `apiVersion` field of the signed structs.
pub const SIGNING_API_VERSION: u32 = 1;

pub const HTTP_TIMEOUT_SECS: u64 = 10;
/// Per-IP limits from the venue docs.
pub const INFO_REQUESTS_PER_SEC: u32 = 50;
pub const EXCHANGE_REQUESTS_PER_SEC: u32 = 10;

pub const MAX_ORDERS_PER_ACTION: usize = 50;
pub const MAX_CANCELS_PER_ACTION: usize = 200;
pub const MAX_CLOID_BYTES: usize = 64;
/// Bars per `candleSnapshot` answer.
pub const MAX_CANDLES: usize = 3000;
/// Rows per page of `historicalOrders` and `userFills`.
pub const MAX_PAGE_ROWS: usize = 2000;

/// The gateway closes a socket after about 16 s without a client frame.
pub const WS_PING_SECS: u64 = 10;
pub const WS_PING: &str = r#"{"method":"ping"}"#;

/// Funding settles every hour.
pub const FUNDING_INTERVAL_MINS: u16 = 60;

pub const ENV_ACCOUNT_ADDRESS: &str = "TXFLOW_ACCOUNT_ADDRESS";
pub const ENV_AGENT_PRIVATE_KEY: &str = "TXFLOW_AGENT_PRIVATE_KEY";
pub const ENV_TESTNET_ACCOUNT_ADDRESS: &str = "TXFLOW_TESTNET_ACCOUNT_ADDRESS";
pub const ENV_TESTNET_AGENT_PRIVATE_KEY: &str = "TXFLOW_TESTNET_AGENT_PRIVATE_KEY";
