//! Order requests of the venue, and what the answer of `/exchange` means for each order.

use anyhow::Context;
use nautilus_model::{
    data::QuoteTick,
    enums::{OrderSide, OrderType, TimeInForce, TriggerType},
    instruments::{Instrument, InstrumentAny},
    orders::{Order, OrderAny},
};
use rust_decimal::Decimal;

use crate::{
    common::{consts::MAX_CLOID_BYTES, enums::TxflowMarginMode, parse::wire_decimal},
    http::{
        error::{TxflowHttpError, TxflowHttpResult},
        models::ExchangeResponse,
    },
    signing::action::{Grouping, OrderBody, OrderKind, Tif, Tpsl},
};

fn slipped(side: OrderSide, price: Decimal, tick: Decimal, slippage_bps: u32) -> Decimal {
    let slip = Decimal::from(slippage_bps) / Decimal::from(10_000);
    match side {
        OrderSide::Buy => (price * (Decimal::ONE + slip) / tick).ceil() * tick,
        _ => (price * (Decimal::ONE - slip) / tick).floor() * tick,
    }
}

/// Price cap of a market order: the opposite top of book plus the slippage, on the tick.
///
/// # Errors
///
/// Returns an error if there is no quote.
pub fn market_price(
    side: OrderSide,
    quote: Option<&QuoteTick>,
    tick: Decimal,
    slippage_bps: u32,
) -> anyhow::Result<Decimal> {
    let quote = quote.context("no quote to cap the market order price")?;
    let top = match side {
        OrderSide::Buy => quote.ask_price,
        _ => quote.bid_price,
    };
    Ok(slipped(side, top.as_decimal(), tick, slippage_bps))
}

/// The venue request of an order and the grouping its action needs.
///
/// # Errors
///
/// Returns an error if the venue cannot take the order as it is.
pub fn order_body(
    order: &OrderAny,
    asset: u32,
    instrument: &InstrumentAny,
    quote: Option<&QuoteTick>,
    slippage_bps: u32,
    margin_mode: TxflowMarginMode,
) -> anyhow::Result<(Grouping, OrderBody)> {
    let client_order_id = order.client_order_id();
    let cloid = client_order_id.as_str();
    anyhow::ensure!(
        cloid.len() <= MAX_CLOID_BYTES,
        "client order id is longer than {MAX_CLOID_BYTES} bytes"
    );
    anyhow::ensure!(
        !cloid.starts_with("0x"),
        "client order id must not start with 0x (reserved by the venue)"
    );
    let side = order.order_side();
    anyhow::ensure!(
        matches!(side, OrderSide::Buy | OrderSide::Sell),
        "order without side"
    );
    anyhow::ensure!(
        !order.is_quote_quantity(),
        "quote quantity is not supported"
    );
    let time_in_force = order.time_in_force();
    anyhow::ensure!(
        time_in_force != TimeInForce::Gtd,
        "GTD is not supported on TxFlow perpetuals"
    );

    let tick = instrument.price_increment().as_decimal();
    let limit_price = || {
        order
            .price()
            .map(|price| price.as_decimal())
            .context("order without price")
    };
    let trigger_price = || {
        anyhow::ensure!(
            matches!(
                order.trigger_type(),
                None | Some(TriggerType::Default | TriggerType::MarkPrice)
            ),
            "TxFlow triggers on the mark price only"
        );
        order
            .trigger_price()
            .map(|price| price.as_decimal())
            .context("trigger order without trigger price")
    };
    let trigger = |is_market: bool, tpsl: Tpsl| -> anyhow::Result<(Decimal, OrderKind)> {
        let trigger_px = trigger_price()?;
        let price = if is_market {
            // unverified: the docs do not say what `p` means on a market trigger; this is the
            // Hyperliquid convention, the worst price the order may fill at once triggered.
            slipped(side, trigger_px, tick, slippage_bps)
        } else {
            limit_price()?
        };
        let kind = OrderKind::Trigger {
            is_market,
            trigger_px: wire_decimal(trigger_px, tick)?,
            tpsl: Some(tpsl),
        };
        Ok((price, kind))
    };

    let (price, kind) = match order.order_type() {
        OrderType::Market => {
            anyhow::ensure!(
                matches!(time_in_force, TimeInForce::Gtc | TimeInForce::Ioc),
                "time in force {time_in_force} is not supported for a market order"
            );
            let price = market_price(side, quote, tick, slippage_bps)?;
            let tif = Tif::FrontendMarket;
            (price, OrderKind::Limit { tif })
        }
        OrderType::Limit => {
            let tif = match time_in_force {
                _ if order.is_post_only() => Tif::Alo,
                TimeInForce::Gtc => Tif::Gtc,
                TimeInForce::Ioc => Tif::Ioc,
                other => anyhow::bail!("time in force {other} is not supported"),
            };
            (limit_price()?, OrderKind::Limit { tif })
        }
        OrderType::StopMarket => trigger(true, Tpsl::Sl)?,
        OrderType::StopLimit => trigger(false, Tpsl::Sl)?,
        OrderType::MarketIfTouched => trigger(true, Tpsl::Tp)?,
        OrderType::LimitIfTouched => trigger(false, Tpsl::Tp)?,
        other => anyhow::bail!("order type {other} is not supported"),
    };
    let grouping = match kind {
        OrderKind::Limit { .. } => Grouping::Na,
        OrderKind::Trigger { .. } => Grouping::NormalTrigger,
    };
    let body = OrderBody {
        a: asset,
        b: side == OrderSide::Buy,
        p: wire_decimal(price, tick)?,
        s: wire_decimal(
            order.quantity().as_decimal(),
            instrument.size_increment().as_decimal(),
        )?,
        r: order.is_reduce_only(),
        t: kind,
        c: Some(cloid.to_string()),
        // Always sent: without it the node uses the mode saved for the market
        m: Some(margin_mode),
        expires_at: None,
    };
    Ok((grouping, body))
}

/// Whether an order list is a parent order with its take-profit and stop-loss: the shape the
/// venue takes as one `normalTpsl` action. The parent is a market or limit order and comes
/// first. Each other order is a trigger order that names it as parent and closes it: same
/// market, opposite side, same quantity. At most one take-profit and one stop-loss.
#[must_use]
pub fn is_tpsl_group(orders: &[OrderAny]) -> bool {
    let [parent, children @ ..] = orders else {
        return false;
    };
    let kinds: Vec<_> = children
        .iter()
        .map(|child| match child.order_type() {
            OrderType::StopMarket | OrderType::StopLimit => Some(Tpsl::Sl),
            OrderType::MarketIfTouched | OrderType::LimitIfTouched => Some(Tpsl::Tp),
            _ => None,
        })
        .collect();
    matches!(parent.order_type(), OrderType::Market | OrderType::Limit)
        && matches!(
            kinds[..],
            [Some(_)] | [Some(Tpsl::Tp), Some(Tpsl::Sl)] | [Some(Tpsl::Sl), Some(Tpsl::Tp)]
        )
        && orders.iter().all(|order| !order.is_closed())
        && children.iter().all(|child| {
            child.parent_order_id() == Some(parent.client_order_id())
                && child.instrument_id() == parent.instrument_id()
                && child.order_side() != parent.order_side()
                && child.quantity() == parent.quantity()
        })
}

/// What the venue said about one order of an `/exchange` action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The action ran. The order stream tells what became of the order.
    Sent,
    /// The venue refused the order: it does not exist.
    Rejected(String),
    /// Nobody knows: the order stream or the next reconciliation decides.
    Unknown(String),
}

/// One verdict for each of the `count` orders of an action.
#[must_use]
pub fn verdicts(result: &TxflowHttpResult<ExchangeResponse>, count: usize) -> Vec<Verdict> {
    let all = |verdict: Verdict| vec![verdict; count];
    let response = match result {
        Ok(response) => response,
        // A rate-limited or refused request did not run; the docs say so for 429
        Err(
            e @ (TxflowHttpError::RateLimit { .. }
            | TxflowHttpError::InvalidRequest(_)
            | TxflowHttpError::Http {
                status: 400..=499, ..
            }),
        ) => return all(Verdict::Rejected(e.to_string())),
        Err(e) => return all(Verdict::Unknown(e.to_string())),
    };
    if response.is_pending() {
        return all(Verdict::Unknown(
            "the node did not confirm the action in time".to_string(),
        ));
    }
    if let Some(reason) = response.action_error() {
        return all(Verdict::Rejected(reason));
    }
    let mut statuses = response.status_errors();
    if statuses.len() == count {
        return statuses
            .into_iter()
            .map(|error| error.map_or(Verdict::Sent, Verdict::Rejected))
            .collect();
    }
    match statuses.pop() {
        // One error for several orders does not say which of them failed
        Some(Some(reason)) if statuses.is_empty() => all(Verdict::Unknown(reason)),
        _ => all(Verdict::Sent),
    }
}

#[cfg(test)]
mod tests {
    use nautilus_model::{
        enums::TriggerType,
        identifiers::{ClientOrderId, InstrumentId},
        orders::builder::OrderTestBuilder,
        types::{Price, Quantity},
    };
    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::http::{models::PerpMeta, parse::parse_instrument};

    fn btc() -> InstrumentAny {
        let meta: PerpMeta = serde_json::from_str(
            r#"{"name":"BTC-USDC","baseCurrency":"BTC","quoteCurrency":"USDC","basePrecision":"0.0001","priceTick":"0.1","maxLeverage":75,"index":1}"#,
        )
        .unwrap();
        parse_instrument(&meta, 0.into()).unwrap()
    }

    fn quote(instrument: &InstrumentAny) -> QuoteTick {
        QuoteTick::new(
            instrument.id(),
            Price::new(99_990.0, instrument.price_precision()),
            Price::new(100_000.0, instrument.price_precision()),
            Quantity::new(1.0, instrument.size_precision()),
            Quantity::new(1.0, instrument.size_precision()),
            0.into(),
            0.into(),
        )
    }

    fn builder(order_type: OrderType) -> OrderTestBuilder {
        let mut builder = OrderTestBuilder::new(order_type);
        builder
            .instrument_id(InstrumentId::from("BTC-USDC-PERP.TXFLOW"))
            .client_order_id(ClientOrderId::from("O-1"))
            .side(OrderSide::Buy)
            .quantity(Quantity::from("0.01"));
        builder
    }

    fn body(order: &OrderAny) -> anyhow::Result<(Grouping, OrderBody)> {
        let instrument = btc();
        let quote = quote(&instrument);
        order_body(
            order,
            1,
            &instrument,
            Some(&quote),
            50,
            TxflowMarginMode::Cross,
        )
    }

    #[test]
    fn limit_order_keeps_price_size_and_flags() {
        let order = builder(OrderType::Limit)
            .price(Price::from("50000"))
            .post_only(true)
            .reduce_only(true)
            .build();
        let (grouping, body) = body(&order).unwrap();
        assert_eq!(grouping, Grouping::Na);
        assert_eq!(body.a, 1);
        assert!(body.b && body.r);
        assert_eq!(body.s, "0.0100");
        assert_eq!(body.p.parse::<Decimal>().unwrap(), Decimal::from(50_000));
        assert_eq!(body.t, OrderKind::Limit { tif: Tif::Alo });
        assert_eq!(body.c.as_deref(), Some("O-1"));
        assert_eq!(body.m, Some(TxflowMarginMode::Cross));
    }

    #[test]
    fn market_order_is_capped_past_the_opposite_top() {
        let (_, buy) = body(&builder(OrderType::Market).build()).unwrap();
        assert_eq!(
            buy.t,
            OrderKind::Limit {
                tif: Tif::FrontendMarket
            }
        );
        // 100000 ask + 50 bps
        assert_eq!(buy.p.parse::<Decimal>().unwrap(), Decimal::from(100_500));
        let sell = builder(OrderType::Market).side(OrderSide::Sell).build();
        let (_, sell) = body(&sell).unwrap();
        assert!(sell.p.parse::<Decimal>().unwrap() < Decimal::from(99_990));
    }

    #[test]
    fn market_order_without_quote_is_refused() {
        let order = builder(OrderType::Market).build();
        let error = order_body(&order, 1, &btc(), None, 50, TxflowMarginMode::Cross).unwrap_err();
        assert!(error.to_string().contains("no quote"));
    }

    #[test]
    fn stop_market_becomes_a_trigger_order() {
        let order = builder(OrderType::StopMarket)
            .side(OrderSide::Sell)
            .trigger_price(Price::from("48000"))
            .trigger_type(TriggerType::MarkPrice)
            .build();
        let (grouping, body) = body(&order).unwrap();
        assert_eq!(grouping, Grouping::NormalTrigger);
        let OrderKind::Trigger {
            is_market,
            trigger_px,
            tpsl,
        } = body.t
        else {
            panic!("not a trigger order");
        };
        assert!(is_market && !body.b);
        assert_eq!(
            trigger_px.parse::<Decimal>().unwrap(),
            Decimal::from(48_000)
        );
        assert_eq!(tpsl, Some(Tpsl::Sl));
        // 48000 - 50 bps: the worst price after the trigger
        assert_eq!(body.p.parse::<Decimal>().unwrap(), Decimal::from(47_760));
    }

    #[rstest]
    #[case::gtd(
        builder(OrderType::Limit)
            .price(Price::from("50000"))
            .time_in_force(TimeInForce::Gtd)
            .expire_time(1_900_000_000_000_000_000.into())
            .build(),
        "GTD"
    )]
    #[case::fok(
        builder(OrderType::Limit)
            .price(Price::from("50000"))
            .time_in_force(TimeInForce::Fok)
            .build(),
        "FOK"
    )]
    #[case::reserved_prefix(
        builder(OrderType::Limit)
            .price(Price::from("50000"))
            .client_order_id(ClientOrderId::from("0xabc"))
            .build(),
        "0x"
    )]
    #[case::long_id(
        builder(OrderType::Limit)
            .price(Price::from("50000"))
            .client_order_id(ClientOrderId::from("O".repeat(65).as_str()))
            .build(),
        "longer"
    )]
    #[case::last_price_trigger(
        builder(OrderType::StopMarket)
            .trigger_price(Price::from("48000"))
            .trigger_type(TriggerType::LastPrice)
            .build(),
        "mark price"
    )]
    #[case::off_tick(
        builder(OrderType::Limit).price(Price::from("50000.123456789")).build(),
        "multiple"
    )]
    fn unsupported_orders_are_refused(#[case] order: OrderAny, #[case] reason: &str) {
        let error = body(&order).unwrap_err().to_string();
        assert!(error.contains(reason), "{error}");
    }

    #[test]
    fn tpsl_group_is_a_parent_with_its_closing_triggers() {
        let parent = builder(OrderType::Limit)
            .price(Price::from("50000"))
            .build();
        let child = |order_type, id: &str| {
            let mut child = builder(order_type);
            child
                .client_order_id(ClientOrderId::from(id))
                .side(OrderSide::Sell)
                .trigger_price(Price::from("48000"))
                .parent_order_id(ClientOrderId::from("O-1"));
            child
        };
        let stop = child(OrderType::StopMarket, "O-2").build();
        let take = child(OrderType::MarketIfTouched, "O-3").build();
        let group = |orders: &[&OrderAny]| {
            is_tpsl_group(
                &orders
                    .iter()
                    .map(|order| (*order).clone())
                    .collect::<Vec<_>>(),
            )
        };

        assert!(group(&[&parent, &stop, &take]));
        assert!(group(&[&parent, &take]));
        // No children, two stops, the parent not first
        assert!(!group(&[&parent]));
        assert!(!group(&[&parent, &stop, &stop]));
        assert!(!group(&[&stop, &parent]));
        // A plain limit take-profit (the default bracket) is not a venue TP/SL child
        let limit_take = child(OrderType::Limit, "O-4")
            .price(Price::from("52000"))
            .build();
        assert!(!group(&[&parent, &stop, &limit_take]));
        // A child of another order, on the side of the parent, or of another size
        let orphan = child(OrderType::StopMarket, "O-5")
            .parent_order_id(ClientOrderId::from("O-9"))
            .build();
        let same_side = child(OrderType::StopMarket, "O-6")
            .side(OrderSide::Buy)
            .build();
        let smaller = child(OrderType::StopMarket, "O-7")
            .quantity(Quantity::from("0.005"))
            .build();
        for odd in [&orphan, &same_side, &smaller] {
            assert!(!group(&[&parent, odd]));
        }
    }

    fn response(value: serde_json::Value) -> TxflowHttpResult<ExchangeResponse> {
        Ok(serde_json::from_value(value).unwrap())
    }

    fn ok(statuses: serde_json::Value) -> TxflowHttpResult<ExchangeResponse> {
        response(json!({
            "status": "ok",
            "code": 0,
            "data": {"type": "PlaceOrder", "data": {"statuses": statuses}},
        }))
    }

    #[test]
    fn verdicts_follow_the_answer() {
        let rejected = |reason: &str| Verdict::Rejected(reason.to_string());
        let unknown = |reason: &str| Verdict::Unknown(reason.to_string());

        // One status for the whole action
        assert_eq!(verdicts(&ok(json!(["success"])), 2), vec![Verdict::Sent; 2]);
        assert_eq!(
            verdicts(&ok(json!([{"error": "no margin"}])), 1),
            [rejected("no margin")]
        );
        assert_eq!(
            verdicts(&ok(json!([{"error": "no margin"}])), 2),
            [unknown("no margin"), unknown("no margin")]
        );
        // One status per order
        assert_eq!(
            verdicts(&ok(json!(["success", {"error": "post only"}])), 2),
            [Verdict::Sent, rejected("post only")]
        );
        assert_eq!(
            verdicts(
                &response(json!({"status": "err", "code": 10000, "message": "bad nonce"})),
                2
            ),
            [rejected("bad nonce"), rejected("bad nonce")]
        );
        assert!(matches!(
            verdicts(&response(json!({"status": "pending", "txHash": "0x1"})), 1)[..],
            [Verdict::Unknown(_)]
        ));
        let rate_limit = Err(TxflowHttpError::RateLimit {
            message: "slow down".to_string(),
            retry_after: None,
        });
        assert!(matches!(
            verdicts(&rate_limit, 1)[..],
            [Verdict::Rejected(_)]
        ));
        let network = Err(TxflowHttpError::Network("reset".to_string()));
        assert!(matches!(verdicts(&network, 1)[..], [Verdict::Unknown(_)]));
        let server = Err(TxflowHttpError::Http {
            status: 502,
            body: String::new(),
            retry_after: None,
        });
        assert!(matches!(verdicts(&server, 1)[..], [Verdict::Unknown(_)]));
    }
}
