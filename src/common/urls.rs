//! Base URLs per TxFlow environment. One host serves `/info`, `/exchange`, `/ws` and `/api-ws`.

use crate::common::enums::TxflowEnvironment;

#[must_use]
pub const fn txflow_http_base_url(environment: TxflowEnvironment) -> &'static str {
    match environment {
        TxflowEnvironment::Mainnet => "https://rpc.txflow.com",
        TxflowEnvironment::Testnet => "https://rpc.txflow-testnet.xyz",
    }
}

/// Node stream: books, trades, quotes, asset context, user streams.
#[must_use]
pub const fn txflow_ws_url(environment: TxflowEnvironment) -> &'static str {
    match environment {
        TxflowEnvironment::Mainnet => "wss://rpc.txflow.com/ws",
        TxflowEnvironment::Testnet => "wss://rpc.txflow-testnet.xyz/ws",
    }
}

/// API-service stream: candles.
#[must_use]
pub const fn txflow_api_ws_url(environment: TxflowEnvironment) -> &'static str {
    match environment {
        TxflowEnvironment::Mainnet => "wss://rpc.txflow.com/api-ws",
        TxflowEnvironment::Testnet => "wss://rpc.txflow-testnet.xyz/api-ws",
    }
}

/// Derives the two socket URLs from an overridden HTTP base URL (tests and proxies).
#[must_use]
pub fn ws_urls_from_http(base_url_http: &str) -> (String, String) {
    let base = base_url_http
        .trim_end_matches('/')
        .replacen("http", "ws", 1);
    (format!("{base}/ws"), format!("{base}/api-ws"))
}
