//! Python bindings of the client factories.

use pyo3::prelude::*;

use crate::{
    common::consts::TXFLOW,
    factories::{data::TxflowDataClientFactory, exec::TxflowExecutionClientFactory},
};

#[pymethods]
impl TxflowDataClientFactory {
    #[new]
    fn py_new() -> Self {
        Self
    }

    #[pyo3(name = "name")]
    fn py_name(&self) -> &'static str {
        TXFLOW
    }
}

#[pymethods]
impl TxflowExecutionClientFactory {
    #[new]
    fn py_new() -> Self {
        Self
    }

    #[pyo3(name = "name")]
    fn py_name(&self) -> &'static str {
        TXFLOW
    }
}
