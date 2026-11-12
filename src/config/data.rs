//! Data client configuration.

use std::any::Any;

use nautilus_common::factories::ClientConfig;
use serde::{Deserialize, Serialize};

use crate::common::{consts::HTTP_TIMEOUT_SECS, enums::TxflowEnvironment};

/// Configuration of the TxFlow data client. Market data is public: no credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_txflow", from_py_object)
)]
pub struct TxflowDataClientConfig {
    pub environment: TxflowEnvironment,
    /// Replaces the base URL of the environment for HTTP and both WebSocket streams.
    pub base_url_http: Option<String>,
    pub proxy_url: Option<String>,
    pub http_timeout_secs: u64,
    /// How often `/info perpMeta` is read for the instrument status (halted, delisted).
    pub instrument_status_poll_secs: u64,
}

impl Default for TxflowDataClientConfig {
    fn default() -> Self {
        Self {
            environment: TxflowEnvironment::default(),
            base_url_http: None,
            proxy_url: None,
            http_timeout_secs: HTTP_TIMEOUT_SECS,
            instrument_status_poll_secs: 60,
        }
    }
}

impl ClientConfig for TxflowDataClientConfig {
    fn as_any(&self) -> &dyn Any {
        self
    }
}
