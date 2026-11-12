//! Live execution test of the TxFlow adapter with the Nautilus `ExecTester`.
//!
//! Edit the constants below to change the instrument and the order size.
//!
//! Run with: `cargo run --example exec_tester --features examples`
//!
//! Credentials are read from the environment at run time (a `.env` file works):
//! `TXFLOW_ACCOUNT_ADDRESS` and `TXFLOW_AGENT_PRIVATE_KEY` (an agent key, never the
//! account key).
//!
//! UNVERIFIED: this example compiles but has never been run. Signing and execution of this
//! adapter have not been checked against the venue (see `docs/STATUS.md`).

use log::LevelFilter;
use nautilus_common::{enums::Environment, logging::logger::LoggerConfig};
use nautilus_live::{config::LiveExecutionEngineConfig, node::LiveNode};
use nautilus_model::{
    identifiers::{ClientId, InstrumentId, StrategyId, TraderId},
    types::Quantity,
};
use nautilus_testkit::testers::{ExecTester, ExecTesterConfig};
use nautilus_txflow::{
    common::consts::TXFLOW,
    config::{data::TxflowDataClientConfig, exec::TxflowExecClientConfig},
    factories::{data::TxflowDataClientFactory, exec::TxflowExecutionClientFactory},
};

// WARNING: with `DRY_RUN = false` this tester submits orders on mainnet and uses
// real funds. With `true` it connects and reconciles without sending orders.
const DRY_RUN: bool = true;
const INSTRUMENT_ID: &str = "BTC-USDC-PERP.TXFLOW";
// BTC: size step 0.0001.
const ORDER_QTY: &str = "0.001";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let instrument_id = InstrumentId::from(INSTRUMENT_ID);
    let log_config = LoggerConfig {
        stdout_level: LevelFilter::Info,
        ..Default::default()
    };
    let exec_engine_config = LiveExecutionEngineConfig {
        reconciliation_lookback_mins: Some(2 * 24 * 60),
        reconciliation_instrument_ids: Some(vec![instrument_id.to_string()]),
        open_check_interval_secs: Some(10.0),
        position_check_interval_secs: Some(30.0),
        ..Default::default()
    };

    let mut node = LiveNode::builder(TraderId::from("TESTER-001"), Environment::Live)?
        .with_name("TXFLOW-EXEC-TESTER-001".to_string())
        .with_logging(log_config)
        .with_exec_engine_config(exec_engine_config)
        // The tester quotes off the top of book, so it needs the data client too.
        .add_data_client(
            Some(TXFLOW.to_string()),
            Box::new(TxflowDataClientFactory),
            Box::new(TxflowDataClientConfig::default()),
        )?
        .add_exec_client(
            Some(TXFLOW.to_string()),
            Box::new(TxflowExecutionClientFactory),
            Box::new(TxflowExecClientConfig::default()),
        )?
        .with_reconciliation(true)
        .with_delay_post_stop_secs(5)
        .build()?;

    let mut tester_config = ExecTesterConfig::builder()
        .instrument_id(instrument_id)
        .client_id(ClientId::new(TXFLOW))
        .order_qty(Quantity::from(ORDER_QTY))
        .dry_run(DRY_RUN)
        .subscribe_quotes(true)
        .subscribe_trades(false)
        .subscribe_book(false)
        .enable_limit_buys(true)
        .enable_limit_sells(true)
        .enable_stop_buys(false)
        .enable_stop_sells(false)
        .tob_offset_ticks(100)
        .use_post_only(true)
        .cancel_orders_on_stop(true)
        .close_positions_on_stop(true)
        .log_data(false)
        .build()?;
    tester_config.base.strategy_id = Some(StrategyId::from("EXEC_TESTER-001"));
    tester_config.base.external_order_instrument_ids = Some(vec![instrument_id]);

    node.add_strategy(ExecTester::new(tester_config))?;
    node.run().await?;

    Ok(())
}
