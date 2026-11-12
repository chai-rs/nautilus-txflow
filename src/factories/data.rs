//! Data client factory.

use std::{cell::RefCell, rc::Rc};

use nautilus_common::{
    cache::CacheView,
    clients::DataClient,
    clock::Clock,
    factories::{ClientConfig, DataClientFactory},
};
use nautilus_model::identifiers::ClientId;

use crate::{common::consts::TXFLOW, config::data::TxflowDataClientConfig, data::TxflowDataClient};

/// Creates [`TxflowDataClient`]s for the live node.
#[derive(Debug, Clone, Default)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_txflow", from_py_object)
)]
pub struct TxflowDataClientFactory;

impl TxflowDataClientFactory {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DataClientFactory for TxflowDataClientFactory {
    fn create(
        &self,
        name: &str,
        config: &dyn ClientConfig,
        _cache: CacheView,
        _clock: Rc<RefCell<dyn Clock>>,
    ) -> anyhow::Result<Box<dyn DataClient>> {
        let config = config
            .as_any()
            .downcast_ref::<TxflowDataClientConfig>()
            .ok_or_else(|| {
                anyhow::anyhow!("Invalid config type for TxflowDataClientFactory: {config:?}")
            })?
            .clone();
        Ok(Box::new(TxflowDataClient::new(
            ClientId::from(name),
            config,
        )?))
    }

    fn name(&self) -> &'static str {
        TXFLOW
    }

    fn config_type(&self) -> &'static str {
        stringify!(TxflowDataClientConfig)
    }
}
