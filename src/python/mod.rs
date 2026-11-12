//! Python bindings: configurations, enums and factories.
//!
//! The pattern is the one of `nautilus-risex`. No stub generation: the stubs
//! serve the official `nautilus_trader` wheel only.

pub mod config;
pub mod factories;

use nautilus_common::factories::{ClientConfig, DataClientFactory, ExecutionClientFactory};
use nautilus_core::python::{to_pyruntime_err, to_pyvalue_err};
use nautilus_system::get_global_pyo3_registry;
use pyo3::prelude::*;

use crate::{
    common::{
        consts::TXFLOW,
        enums::{TxflowEnvironment, TxflowMarginMode},
    },
    config::{data::TxflowDataClientConfig, exec::TxflowExecClientConfig},
    factories::{data::TxflowDataClientFactory, exec::TxflowExecutionClientFactory},
};

#[expect(clippy::needless_pass_by_value)]
fn extract_data_factory(
    py: Python<'_>,
    factory: Py<PyAny>,
) -> PyResult<Box<dyn DataClientFactory>> {
    match factory.extract::<TxflowDataClientFactory>(py) {
        Ok(f) => Ok(Box::new(f)),
        Err(e) => Err(to_pyvalue_err(format!(
            "Failed to extract TxflowDataClientFactory: {e}"
        ))),
    }
}

#[expect(clippy::needless_pass_by_value)]
fn extract_exec_factory(
    py: Python<'_>,
    factory: Py<PyAny>,
) -> PyResult<Box<dyn ExecutionClientFactory>> {
    match factory.extract::<TxflowExecutionClientFactory>(py) {
        Ok(f) => Ok(Box::new(f)),
        Err(e) => Err(to_pyvalue_err(format!(
            "Failed to extract TxflowExecutionClientFactory: {e}"
        ))),
    }
}

#[expect(clippy::needless_pass_by_value)]
fn extract_data_config(py: Python<'_>, config: Py<PyAny>) -> PyResult<Box<dyn ClientConfig>> {
    match config.extract::<TxflowDataClientConfig>(py) {
        Ok(c) => Ok(Box::new(c)),
        Err(e) => Err(to_pyvalue_err(format!(
            "Failed to extract TxflowDataClientConfig: {e}"
        ))),
    }
}

#[expect(clippy::needless_pass_by_value)]
fn extract_exec_config(py: Python<'_>, config: Py<PyAny>) -> PyResult<Box<dyn ClientConfig>> {
    match config.extract::<TxflowExecClientConfig>(py) {
        Ok(c) => Ok(Box::new(c)),
        Err(e) => Err(to_pyvalue_err(format!(
            "Failed to extract TxflowExecClientConfig: {e}"
        ))),
    }
}

/// Adds the TxFlow classes to `m` and registers the extractors of the live node.
///
/// # Errors
///
/// Returns an error if a class or an extractor cannot be registered.
#[pymodule]
pub fn txflow(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add(stringify!(TXFLOW), TXFLOW)?;
    m.add_class::<TxflowEnvironment>()?;
    m.add_class::<TxflowMarginMode>()?;
    m.add_class::<TxflowDataClientConfig>()?;
    m.add_class::<TxflowDataClientFactory>()?;
    m.add_class::<TxflowExecClientConfig>()?;
    m.add_class::<TxflowExecutionClientFactory>()?;

    let registry = get_global_pyo3_registry();
    registry
        .register_factory_extractor(TXFLOW.to_string(), extract_data_factory)
        .map_err(to_pyruntime_err)?;
    registry
        .register_exec_factory_extractor(TXFLOW.to_string(), extract_exec_factory)
        .map_err(to_pyruntime_err)?;
    registry
        .register_config_extractor("TxflowDataClientConfig".to_string(), extract_data_config)
        .map_err(to_pyruntime_err)?;
    registry
        .register_config_extractor("TxflowExecClientConfig".to_string(), extract_exec_config)
        .map_err(to_pyruntime_err)?;
    Ok(())
}
