//! Flattens a TxFlow account: cancels every open order, every pending trigger order and every
//! pending TP/SL order, then closes every position with a reduce-only IOC order.
//!
//! The plan is printed first and nothing is sent before Enter is pressed.
//!
//! Environment selection: pass `testnet` as the first argv to target testnet;
//! defaults to mainnet.
//!
//! Environment variables (read at run time only, never printed):
//! - `TXFLOW_ACCOUNT_ADDRESS`, `TXFLOW_AGENT_PRIVATE_KEY`
//!   (or the `TXFLOW_TESTNET_*` variants).
//!
//! Run with:
//!
//! ```bash
//! cargo run --bin txflow-flatten           # mainnet
//! cargo run --bin txflow-flatten testnet   # testnet
//! ```

use std::{
    collections::HashSet,
    env,
    io::{BufRead, Write},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::Address;
use anyhow::Context;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::QuoteTick,
    enums::OrderSide,
    instruments::{Instrument, InstrumentAny},
};
use nautilus_txflow::{
    common::{
        consts::{
            ENV_ACCOUNT_ADDRESS, ENV_AGENT_PRIVATE_KEY, ENV_TESTNET_ACCOUNT_ADDRESS,
            ENV_TESTNET_AGENT_PRIVATE_KEY, MAX_CANCELS_PER_ACTION,
        },
        enums::{TxflowEnvironment, TxflowMarginMode},
        instruments::InstrumentCache,
        parse::{price_from_decimal, quantity_from_decimal, wire_decimal},
    },
    config::exec::TxflowExecClientConfig,
    execution::order::{Verdict, market_price, verdicts},
    http::{
        client::TxflowHttpClient,
        models::{ClearinghouseState, L2Book, Position, TxflowOrder},
    },
    signing::{
        TxflowSigner,
        action::{
            Action, CancelAction, CancelBody, CancelGrouping, Grouping, OrderAction, OrderBody,
            OrderKind, Tif,
        },
        nonce::NonceManager,
    },
};
use rust_decimal::Decimal;
use serde_json::json;

/// A position to close. `szi` is the signed size: positive is long.
#[derive(Debug)]
struct Close {
    asset: u32,
    instrument: InstrumentAny,
    szi: Decimal,
    /// The margin mode is part of the position key: the order names the position it reduces.
    margin_mode: Option<TxflowMarginMode>,
}

/// Margin mode of a position, from `leverage.type`.
// unverified: the docs list no values of `leverage.type`; "Cross" and "Isolated" are read
// without case. With another value the order has no `m`, and the node then takes the margin
// mode saved for the market.
fn margin_mode(position: &Position) -> Option<TxflowMarginMode> {
    let kind = position.leverage["type"].as_str()?;
    match kind.to_ascii_lowercase().as_str() {
        "cross" => Some(TxflowMarginMode::Cross),
        "isolated" => Some(TxflowMarginMode::Isolated),
        _ => None,
    }
}

/// The asset index and the instrument of a venue row.
fn market(cache: &InstrumentCache, key: &str) -> anyhow::Result<(u32, InstrumentAny)> {
    cache
        .lookup(key)
        .and_then(|instrument| Some((cache.asset(&instrument.id())?, instrument)))
        .with_context(|| format!("market {key} is not in the instrument list"))
}

/// One cancel for each resting order, pending trigger order and pending TP/SL leg.
///
/// # Errors
///
/// Returns an error if an order is on a market that is not in `cache`: nothing is sent then.
fn cancels(
    cache: &InstrumentCache,
    open: &[TxflowOrder],
    triggers: &[TxflowOrder],
    tpsl: &[TxflowOrder],
) -> anyhow::Result<Vec<CancelBody>> {
    let mut listed = HashSet::new();
    let mut cancels = Vec::new();
    // unverified: the docs do not say whether one order shows in two of the three lists. The
    // most specific list wins, so that each order is cancelled once, with its own family.
    let families = [(tpsl, true), (triggers, false), (open, false)];
    for (orders, is_tpsl) in families {
        for order in orders.iter().filter(|order| listed.insert(order.oid)) {
            let (a, _) = market(cache, &order.instrument_key())
                .with_context(|| format!("order {}", order.oid))?;
            let grouping = if is_tpsl {
                Some(CancelGrouping::Tpsl)
            } else {
                order.is_trigger.then_some(CancelGrouping::Trigger)
            };
            cancels.push(CancelBody {
                a,
                o: order.oid,
                grouping,
            });
        }
    }
    Ok(cancels)
}

/// The open positions of the account.
///
/// # Errors
///
/// Returns an error if a position is on a market that is not in `cache`: nothing is sent then.
fn closes(cache: &InstrumentCache, state: &ClearinghouseState) -> anyhow::Result<Vec<Close>> {
    state
        .asset_positions
        .iter()
        .filter_map(|row| {
            let szi = row.position.szi.filter(|szi| !szi.is_zero())?;
            let market = market(cache, &row.position.instrument_key());
            Some(market.map(|(asset, instrument)| Close {
                asset,
                instrument,
                szi,
                margin_mode: margin_mode(&row.position),
            }))
        })
        .collect()
}

/// The reduce-only IOC order that closes `close`: a sell at the best bid minus the slippage
/// for a long position, a buy at the best ask plus the slippage for a short one.
///
/// # Errors
///
/// Returns an error if a side of the book is empty or a value does not fit the instrument.
fn close_order(close: &Close, book: &L2Book, slippage_bps: u32) -> anyhow::Result<OrderBody> {
    let instrument = &close.instrument;
    let (bids, asks) = &book.levels;
    let bid = bids.iter().max_by_key(|level| level.px);
    let ask = asks.iter().min_by_key(|level| level.px);
    let (bid, ask) = bid.zip(ask).context("book has no bid or no ask")?;
    let (price_precision, size_precision) =
        (instrument.price_precision(), instrument.size_precision());
    let quote = QuoteTick::new(
        instrument.id(),
        price_from_decimal(bid.px, price_precision)?,
        price_from_decimal(ask.px, price_precision)?,
        quantity_from_decimal(bid.sz, size_precision)?,
        quantity_from_decimal(ask.sz, size_precision)?,
        UnixNanos::default(),
        UnixNanos::default(),
    );
    let side = if close.szi.is_sign_positive() {
        OrderSide::Sell
    } else {
        OrderSide::Buy
    };
    let tick = instrument.price_increment().as_decimal();
    Ok(OrderBody {
        a: close.asset,
        b: side == OrderSide::Buy,
        p: wire_decimal(market_price(side, Some(&quote), tick, slippage_bps)?, tick)?,
        s: wire_decimal(close.szi.abs(), instrument.size_increment().as_decimal())?,
        r: true,
        t: OrderKind::Limit { tif: Tif::Ioc },
        c: None,
        m: close.margin_mode,
        expires_at: None,
    })
}

/// Signs and sends one action of `count` items.
///
/// # Errors
///
/// Returns an error unless the venue ran the action for every item.
async fn send(
    http: &TxflowHttpClient,
    signer: &TxflowSigner,
    nonces: &NonceManager,
    action: &Action,
    count: usize,
) -> anyhow::Result<()> {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let envelope = signer.envelope(action, nonces.next(now_ms))?;
    let result = http.exchange(&envelope).await;
    for verdict in verdicts(&result, count) {
        match verdict {
            Verdict::Sent => {}
            Verdict::Rejected(reason) => anyhow::bail!("refused: {reason}"),
            Verdict::Unknown(reason) => anyhow::bail!("no clear answer: {reason}"),
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nautilus_common::logging::ensure_logging_initialized();

    let (environment, address_var, key_var) =
        if env::args().nth(1).is_some_and(|arg| arg == "testnet") {
            (
                TxflowEnvironment::Testnet,
                ENV_TESTNET_ACCOUNT_ADDRESS,
                ENV_TESTNET_AGENT_PRIVATE_KEY,
            )
        } else {
            (
                TxflowEnvironment::Mainnet,
                ENV_ACCOUNT_ADDRESS,
                ENV_AGENT_PRIVATE_KEY,
            )
        };
    let address = env::var(address_var).with_context(|| format!("{address_var} is not set"))?;
    let address = Address::from_str(address.trim()).context("account address is not valid")?;
    let user = format!("{address:#x}");
    let key = env::var(key_var).with_context(|| format!("{key_var} is not set"))?;
    let signer = TxflowSigner::new(&key, environment)?;
    let defaults = TxflowExecClientConfig::default();

    let http = TxflowHttpClient::new(environment, None, defaults.http_timeout_secs, None)?;
    http.request_instruments().await?;
    let cache = http.instruments();

    let open = http.request_open_orders(&user).await?;
    let triggers = http.request_trigger_orders(&user).await?;
    let tpsl = match http.request_tpsl_orders(&user).await {
        Ok(legs) => legs,
        // unverified: `tpslOrders` was never read live. An answer the adapter cannot use must
        // not stop the flatten of everything else.
        Err(e) => {
            println!("TP/SL orders could not be listed ({e}): check them by hand");
            Vec::new()
        }
    };
    let state = http.request_clearinghouse_state(&user).await?;
    let cancels = cancels(cache, &open, &triggers, &tpsl)?;
    let closes = closes(cache, &state)?;

    println!();
    println!("TxFlow flatten");
    println!("  environment     : {environment:?}");
    println!("  account         : {user}");
    for cancel in &cancels {
        let family = cancel.grouping.map_or("order", |grouping| match grouping {
            CancelGrouping::Trigger => "trigger order",
            CancelGrouping::Tpsl => "TP/SL order",
        });
        println!(
            "  cancel          : asset {} {family} {}",
            cancel.a, cancel.o
        );
    }
    for close in &closes {
        println!(
            "  close position  : {} {} {} (reduce-only IOC, {} bps past the book)",
            close.instrument.id(),
            close.szi,
            close
                .margin_mode
                .map_or("margin mode not read", |mode| match mode {
                    TxflowMarginMode::Cross => "cross",
                    TxflowMarginMode::Isolated => "isolated",
                }),
            defaults.market_order_slippage_bps
        );
    }
    if cancels.is_empty() && closes.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }
    print!("\nProceed? [Enter to continue, Ctrl+C to abort] ");
    std::io::stdout().flush()?;
    std::io::stdin().lock().read_line(&mut String::new())?;

    let nonces = NonceManager::default();
    let mut failed = 0;
    let mut report = |what: String, result: anyhow::Result<()>| match result {
        Ok(()) => println!("{what}: ok"),
        Err(e) => {
            println!("{what}: FAILED {e:#}");
            failed += 1;
        }
    };

    // Orders go first: a resting order can block a reduce-only close.
    for chunk in cancels.chunks(MAX_CANCELS_PER_ACTION) {
        let action = Action::Cancel(CancelAction {
            cancels: chunk.to_vec(),
        });
        let result = send(&http, &signer, &nonces, &action, chunk.len()).await;
        report(format!("cancel {} order(s)", chunk.len()), result);
    }
    for close in &closes {
        let result = async {
            let coin = close.asset.to_string();
            let book: L2Book = http.info(&json!({"type": "l2Book", "coin": coin})).await?;
            let order = close_order(close, &book, defaults.market_order_slippage_bps)?;
            let action = Action::Order(OrderAction {
                grouping: Grouping::Na,
                orders: vec![order],
            });
            send(&http, &signer, &nonces, &action, 1).await
        }
        .await;
        report(format!("close position, {}", close.instrument.id()), result);
    }

    // shortcut: one IOC order per position, so a thin book leaves a rest. Run again until
    // it prints "Nothing to do"; upgrade to a fill-checking loop if that is too manual.
    anyhow::ensure!(
        failed == 0,
        "{failed} step(s) failed, check the account and run again"
    );
    println!("Sent. Run again to confirm the account is flat.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use nautilus_txflow::http::{
        models::{InfoEnvelope, TpslPair, Universe},
        parse::parse_instrument,
    };
    use rstest::rstest;
    use rust_decimal_macros::dec;
    use serde::de::DeserializeOwned;

    use super::*;

    const PERP_META: &str = include_str!("../tests/fixtures/perpMeta.json");
    const OPEN_ORDERS: &str = include_str!("../tests/fixtures/frontendOpenOrders.json");
    const TRIGGER_ORDERS: &str = include_str!("../tests/fixtures/triggerOrders.json");
    const STATE: &str = include_str!("../tests/fixtures/clearinghouseState.json");
    const BOOK: &str = include_str!("../tests/fixtures/l2Book.json");

    fn data<T: DeserializeOwned>(fixture: &str) -> T {
        let envelope: InfoEnvelope<T> = serde_json::from_str(fixture).unwrap();
        envelope.data.unwrap()
    }

    fn cache() -> InstrumentCache {
        let cache = InstrumentCache::default();
        for market in data::<Universe>(PERP_META).universe {
            let instrument = parse_instrument(&market, UnixNanos::default()).unwrap();
            cache.insert(
                market.index,
                &market.name,
                &market.base_currency,
                instrument,
            );
        }
        cache
    }

    fn triggers() -> Vec<TxflowOrder> {
        let mut triggers: Vec<TxflowOrder> = data(TRIGGER_ORDERS);
        // As `request_trigger_orders` marks them
        triggers
            .iter_mut()
            .for_each(|order| order.is_trigger = true);
        triggers
    }

    fn tpsl_legs() -> Vec<TxflowOrder> {
        let pair: TpslPair = serde_json::from_value(json!({
            "coin": "ETH-USDC", "asset": 2, "side": "A", "quantity": "0.010",
            "slOid": 71, "slTriggerPrice": "3000.00", "tpOid": 72, "tpTriggerPrice": "5000.00",
        }))
        .unwrap();
        pair.legs()
    }

    #[rstest]
    fn plan_cancels_each_order_once_with_its_family() {
        let (open, mut triggers, tpsl) = (
            data::<Vec<TxflowOrder>>(OPEN_ORDERS),
            triggers(),
            tpsl_legs(),
        );
        // The stop-loss leg is also in the trigger list: it is cancelled once, as a TP/SL order
        triggers.push(tpsl[0].clone());

        let cancels = cancels(&cache(), &open, &triggers, &tpsl).unwrap();

        assert_eq!(
            serde_json::to_value(&cancels).unwrap(),
            json!([
                {"a": 2, "o": 71, "grouping": "tpsl"},
                {"a": 2, "o": 72, "grouping": "tpsl"},
                {"a": 1, "o": 211_558_908_001_u64, "grouping": "trigger"},
                {"a": 1, "o": 211_558_907_950_u64},
            ])
        );
    }

    #[rstest]
    fn plan_of_an_empty_account_is_empty() {
        let state = ClearinghouseState::default();
        assert!(cancels(&cache(), &[], &[], &[]).unwrap().is_empty());
        assert!(closes(&cache(), &state).unwrap().is_empty());
    }

    #[rstest]
    fn order_or_position_on_an_unknown_market_stops_the_plan() {
        let mut open: Vec<TxflowOrder> = data(OPEN_ORDERS);
        open[0].asset = Some(999);
        let error = cancels(&cache(), &open, &[], &[]).unwrap_err();
        assert!(format!("{error:#}").contains("market 999 is not in the instrument list"));

        let mut state: ClearinghouseState = data(STATE);
        state.asset_positions[0].position.asset = Some(999);
        assert!(closes(&cache(), &state).is_err());
    }

    #[rstest]
    fn plan_closes_positions_that_have_a_size() {
        let mut state: ClearinghouseState = data(STATE);
        let mut flat = state.asset_positions[0].clone();
        flat.position.szi = Some(Decimal::ZERO);
        state.asset_positions.push(flat);

        let closes = closes(&cache(), &state).unwrap();

        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0].asset, 1);
        assert_eq!(
            closes[0].instrument.id().to_string(),
            "BTC-USDC-PERP.TXFLOW"
        );
        assert_eq!(closes[0].szi, dec!(0.0030));
        assert_eq!(closes[0].margin_mode, Some(TxflowMarginMode::Cross));
    }

    #[rstest]
    #[case(json!({"type": "Cross", "value": 10}), Some(TxflowMarginMode::Cross))]
    #[case(json!({"type": "isolated"}), Some(TxflowMarginMode::Isolated))]
    #[case(json!({"type": "portfolio"}), None)]
    #[case(json!(10), None)]
    #[case(json!(null), None)]
    fn margin_mode_is_read_from_the_leverage_of_the_position(
        #[case] leverage: serde_json::Value,
        #[case] expected: Option<TxflowMarginMode>,
    ) {
        let position = Position {
            leverage,
            ..Default::default()
        };
        assert_eq!(margin_mode(&position), expected);
    }

    /// Book of the fixture: best bid 73418.6, best ask 73418.8, tick 0.1; slippage 50 bps.
    #[rstest]
    #[case(dec!(0.0030), json!({"a": 1, "b": false, "p": "73051.5", "s": "0.0030", "r": true, "t": {"limit": {"tif": "Ioc"}}, "m": "cross"}))]
    #[case(dec!(-0.0030), json!({"a": 1, "b": true, "p": "73785.9", "s": "0.0030", "r": true, "t": {"limit": {"tif": "Ioc"}}, "m": "cross"}))]
    fn close_order_is_reduce_only_ioc_past_the_book(
        #[case] szi: Decimal,
        #[case] expected: serde_json::Value,
    ) {
        let mut close = closes(&cache(), &data(STATE)).unwrap().remove(0);
        close.szi = szi;

        let order = close_order(&close, &data(BOOK), 50).unwrap();

        assert_eq!(serde_json::to_value(&order).unwrap(), expected);
    }

    #[rstest]
    fn close_order_needs_a_size_on_the_step_and_both_sides_of_the_book() {
        let mut close = closes(&cache(), &data(STATE)).unwrap().remove(0);
        let mut book: L2Book = data(BOOK);

        close.szi = dec!(0.00305);
        assert!(close_order(&close, &book, 50).is_err());

        close.szi = dec!(0.0030);
        close.margin_mode = None;
        let order = close_order(&close, &book, 50).unwrap();
        assert!(serde_json::to_value(&order).unwrap().get("m").is_none());

        book.levels.0.clear();
        assert!(close_order(&close, &book, 50).is_err());
    }
}
