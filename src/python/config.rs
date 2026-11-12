//! Python bindings of the client configurations.

use std::collections::HashMap;

use nautilus_model::identifiers::{AccountId, InstrumentId};
use pyo3::pymethods;

use crate::{
    common::enums::{TxflowEnvironment, TxflowMarginMode},
    config::{data::TxflowDataClientConfig, exec::TxflowExecClientConfig},
};

#[pymethods]
impl TxflowDataClientConfig {
    #[new]
    #[pyo3(signature = (
        environment = None,
        base_url_http = None,
        proxy_url = None,
        http_timeout_secs = None,
    ))]
    fn py_new(
        environment: Option<TxflowEnvironment>,
        base_url_http: Option<String>,
        proxy_url: Option<String>,
        http_timeout_secs: Option<u64>,
    ) -> Self {
        let defaults = Self::default();
        Self {
            environment: environment.unwrap_or(defaults.environment),
            base_url_http,
            proxy_url,
            http_timeout_secs: http_timeout_secs.unwrap_or(defaults.http_timeout_secs),
        }
    }

    #[getter]
    fn environment(&self) -> TxflowEnvironment {
        self.environment
    }

    #[getter]
    fn base_url_http(&self) -> Option<String> {
        self.base_url_http.clone()
    }

    #[getter]
    fn http_timeout_secs(&self) -> u64 {
        self.http_timeout_secs
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }
}

#[pymethods]
impl TxflowExecClientConfig {
    #[new]
    #[pyo3(signature = (
        account_id = None,
        environment = None,
        base_url_http = None,
        proxy_url = None,
        http_timeout_secs = None,
        account_address = None,
        agent_private_key = None,
        market_order_slippage_bps = None,
        margin_mode = None,
        leverage = None,
    ))]
    #[expect(clippy::too_many_arguments)]
    fn py_new(
        account_id: Option<AccountId>,
        environment: Option<TxflowEnvironment>,
        base_url_http: Option<String>,
        proxy_url: Option<String>,
        http_timeout_secs: Option<u64>,
        account_address: Option<String>,
        agent_private_key: Option<String>,
        market_order_slippage_bps: Option<u32>,
        margin_mode: Option<TxflowMarginMode>,
        leverage: Option<HashMap<InstrumentId, u32>>,
    ) -> Self {
        let defaults = Self::default();
        Self {
            environment: environment.unwrap_or(defaults.environment),
            base_url_http,
            proxy_url,
            http_timeout_secs: http_timeout_secs.unwrap_or(defaults.http_timeout_secs),
            account_id: account_id.unwrap_or(defaults.account_id),
            account_address,
            agent_private_key,
            market_order_slippage_bps: market_order_slippage_bps
                .unwrap_or(defaults.market_order_slippage_bps),
            margin_mode: margin_mode.unwrap_or(defaults.margin_mode),
            leverage: leverage.unwrap_or_default(),
        }
    }

    #[getter]
    fn environment(&self) -> TxflowEnvironment {
        self.environment
    }

    #[getter]
    fn account_id(&self) -> AccountId {
        self.account_id
    }

    #[getter]
    fn account_address(&self) -> Option<String> {
        self.account_address.clone()
    }

    #[getter]
    fn market_order_slippage_bps(&self) -> u32 {
        self.market_order_slippage_bps
    }

    #[getter]
    fn margin_mode(&self) -> TxflowMarginMode {
        self.margin_mode
    }

    // No getter for `agent_private_key`; `__repr__` redacts it.
    fn __repr__(&self) -> String {
        format!("{self:?}")
    }
}
