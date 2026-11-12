//! Python binding tests: an embedded interpreter, no network.

#![cfg(feature = "python")]

use nautilus_model::identifiers::{AccountId, InstrumentId};
use nautilus_system::get_global_pyo3_registry;
use nautilus_txflow::{
    common::{
        consts::TXFLOW,
        enums::{TxflowEnvironment, TxflowMarginMode},
    },
    config::{data::TxflowDataClientConfig, exec::TxflowExecClientConfig},
    factories::{data::TxflowDataClientFactory, exec::TxflowExecutionClientFactory},
    python,
};
use pyo3::{
    Bound, Py, Python,
    types::{PyAnyMethods, PyDict, PyDictMethods, PyModule},
};

const BTC: &str = "BTC-USDC-PERP.TXFLOW";

fn module(py: Python<'_>) -> Bound<'_, PyModule> {
    let module = PyModule::new(py, "txflow").unwrap();
    python::txflow(&module).unwrap();
    module
}

// One test: the global registry accepts each extractor once per process.
#[test]
fn module_registers_classes_and_extractors() {
    Python::initialize();
    Python::attach(|py| {
        let module = module(py);
        configs_construct_from_python(&module);
        factories_and_configs_extract_through_the_registry(py);
    });
}

fn configs_construct_from_python(module: &Bound<'_, PyModule>) {
    let py = module.py();
    let data = module
        .getattr("TxflowDataClientConfig")
        .unwrap()
        .call0()
        .unwrap()
        .extract::<TxflowDataClientConfig>()
        .unwrap();
    assert_eq!(data.environment, TxflowEnvironment::Mainnet);
    assert_eq!(data.http_timeout_secs, 10);

    let exec_class = module.getattr("TxflowExecClientConfig").unwrap();
    let variant = |class: &str, name: &str| module.getattr(class).unwrap().getattr(name).unwrap();
    let kwargs = PyDict::new(py);
    kwargs
        .set_item(
            "account_id",
            Py::new(py, AccountId::from("TXFLOW-007")).unwrap(),
        )
        .unwrap();
    kwargs
        .set_item("environment", variant("TxflowEnvironment", "TESTNET"))
        .unwrap();
    kwargs
        .set_item("margin_mode", variant("TxflowMarginMode", "ISOLATED"))
        .unwrap();
    kwargs.set_item("agent_private_key", "0xdeadbeef").unwrap();
    let leverage = PyDict::new(py);
    leverage
        .set_item(Py::new(py, InstrumentId::from(BTC)).unwrap(), 3)
        .unwrap();
    kwargs.set_item("leverage", leverage).unwrap();

    let object = exec_class.call((), Some(&kwargs)).unwrap();
    let repr = object.repr().unwrap().to_string();
    assert!(!repr.contains("deadbeef"), "{repr}");
    assert!(object.getattr("agent_private_key").is_err());
    let exec = object.extract::<TxflowExecClientConfig>().unwrap();
    assert_eq!(exec.account_id, AccountId::from("TXFLOW-007"));
    assert_eq!(exec.environment, TxflowEnvironment::Testnet);
    assert_eq!(exec.margin_mode, TxflowMarginMode::Isolated);
    assert_eq!(exec.leverage[&InstrumentId::from(BTC)], 3);
    assert_eq!(exec.market_order_slippage_bps, 50);

    kwargs.set_item("margin_mode", "HEDGED").unwrap();
    assert!(exec_class.call((), Some(&kwargs)).is_err());
}

fn factories_and_configs_extract_through_the_registry(py: Python<'_>) {
    let registry = get_global_pyo3_registry();

    let factory = Py::new(py, TxflowDataClientFactory::new())
        .unwrap()
        .into_any();
    let factory = registry.extract_factory(py, factory).unwrap();
    assert_eq!(factory.name(), TXFLOW);
    assert_eq!(factory.config_type(), "TxflowDataClientConfig");

    let factory = Py::new(py, TxflowExecutionClientFactory)
        .unwrap()
        .into_any();
    let factory = registry.extract_exec_factory(py, factory).unwrap();
    assert_eq!(factory.name(), TXFLOW);
    assert_eq!(factory.config_type(), "TxflowExecClientConfig");

    let config = TxflowDataClientConfig {
        http_timeout_secs: 7,
        ..Default::default()
    };
    let config = Py::new(py, config).unwrap().into_any();
    let config = registry.extract_config(py, config).unwrap();
    let config = config
        .as_any()
        .downcast_ref::<TxflowDataClientConfig>()
        .unwrap();
    assert_eq!(config.http_timeout_secs, 7);

    let config = TxflowExecClientConfig {
        market_order_slippage_bps: 9,
        ..Default::default()
    };
    let config = Py::new(py, config).unwrap().into_any();
    let config = registry.extract_config(py, config).unwrap();
    let config = config
        .as_any()
        .downcast_ref::<TxflowExecClientConfig>()
        .unwrap();
    assert_eq!(config.market_order_slippage_bps, 9);
}
