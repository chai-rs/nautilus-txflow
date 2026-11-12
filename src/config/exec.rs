//! Execution client configuration.

use std::{collections::HashMap, fmt};

use nautilus_model::identifiers::{AccountId, InstrumentId};

use crate::common::{
    consts::HTTP_TIMEOUT_SECS,
    enums::{TxflowEnvironment, TxflowMarginMode},
};

/// Configuration of the TxFlow execution client.
#[derive(Clone)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_txflow", from_py_object)
)]
pub struct TxflowExecClientConfig {
    pub environment: TxflowEnvironment,
    /// Replaces the base URL of the environment for HTTP and both WebSocket streams.
    pub base_url_http: Option<String>,
    pub proxy_url: Option<String>,
    pub http_timeout_secs: u64,
    pub account_id: AccountId,
    /// Account (wallet) address; falls back to the environment variable.
    pub account_address: Option<String>,
    /// Agent (API wallet) key; falls back to the environment variable. Never the account key.
    pub agent_private_key: Option<String>,
    /// Price cap of a market order, in basis points past the opposite top of book.
    pub market_order_slippage_bps: u32,
    /// Margin mode sent with every order.
    pub margin_mode: TxflowMarginMode,
    /// Leverage set at connect, per instrument.
    pub leverage: HashMap<InstrumentId, u32>,
}

impl Default for TxflowExecClientConfig {
    fn default() -> Self {
        Self {
            environment: TxflowEnvironment::default(),
            base_url_http: None,
            proxy_url: None,
            http_timeout_secs: HTTP_TIMEOUT_SECS,
            account_id: AccountId::from("TXFLOW-001"),
            account_address: None,
            agent_private_key: None,
            market_order_slippage_bps: 50,
            margin_mode: TxflowMarginMode::default(),
            leverage: HashMap::new(),
        }
    }
}

// Manual impl so the agent key can never reach logs.
impl fmt::Debug for TxflowExecClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxflowExecClientConfig")
            .field("environment", &self.environment)
            .field("base_url_http", &self.base_url_http)
            .field("account_id", &self.account_id)
            .field("account_address", &self.account_address)
            .field(
                "agent_private_key",
                &self.agent_private_key.as_ref().map(|_| "<redacted>"),
            )
            .field("market_order_slippage_bps", &self.market_order_slippage_bps)
            .field("margin_mode", &self.margin_mode)
            .field("leverage", &self.leverage)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_agent_key() {
        let config = TxflowExecClientConfig {
            agent_private_key: Some("0xdeadbeef".to_string()),
            ..Default::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("deadbeef"));
        assert!(debug.contains("<redacted>"));
    }
}
