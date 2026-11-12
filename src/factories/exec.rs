//! Factory for the TxFlow execution client.

use std::{any::Any, cell::RefCell, rc::Rc};

use nautilus_common::{
    cache::CacheView,
    clients::ExecutionClient,
    clock::Clock,
    factories::{ClientConfig, ExecutionClientFactory},
};
use nautilus_live::ExecutionClientCore;
use nautilus_model::{
    enums::{AccountType, OmsType},
    identifiers::{ClientId, TraderId},
};

use crate::{
    common::consts::{TXFLOW, TXFLOW_VENUE},
    config::exec::TxflowExecClientConfig,
    execution::TxflowExecutionClient,
};

impl ClientConfig for TxflowExecClientConfig {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_txflow", from_py_object)
)]
pub struct TxflowExecutionClientFactory;

impl ExecutionClientFactory for TxflowExecutionClientFactory {
    fn create(
        &self,
        trader_id: TraderId,
        name: &str,
        config: &dyn ClientConfig,
        cache: CacheView,
        _clock: Rc<RefCell<dyn Clock>>,
    ) -> anyhow::Result<Box<dyn ExecutionClient>> {
        let config = config
            .as_any()
            .downcast_ref::<TxflowExecClientConfig>()
            .ok_or_else(|| anyhow::anyhow!("expected TxflowExecClientConfig, was {config:?}"))?
            .clone();
        // One-way mode: one position per market, on a margin account.
        let core = ExecutionClientCore::new(
            trader_id,
            ClientId::from(name),
            *TXFLOW_VENUE,
            OmsType::Netting,
            config.account_id,
            AccountType::Margin,
            None,
            cache,
        );
        Ok(Box::new(TxflowExecutionClient::new(core, config)?))
    }

    fn name(&self) -> &'static str {
        TXFLOW
    }

    fn config_type(&self) -> &'static str {
        "TxflowExecClientConfig"
    }
}
