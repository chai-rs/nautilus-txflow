//! Conversion of venue models to Nautilus market data types and execution reports.

use std::collections::HashMap;

use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, BarType, FundingRateUpdate, IndexPriceUpdate, MarkPriceUpdate, TradeTick},
    enums::{MarketStatusAction, OrderStatus, OrderType, PositionSide, TimeInForce, TriggerType},
    identifiers::{AccountId, ClientOrderId, InstrumentId, Symbol, TradeId, VenueOrderId},
    instruments::{CryptoPerpetual, Instrument, InstrumentAny},
    reports::{FillReport, OrderStatusReport, PositionStatusReport},
    types::{AccountBalance, Currency, Money},
};
use rust_decimal::Decimal;

use crate::{
    common::{
        consts::{FUNDING_INTERVAL_MINS, TXFLOW_VENUE},
        enums::{
            parse_aggressor_side, parse_liquidity_side, parse_order_side, parse_order_status,
            parse_time_in_force,
        },
        parse::{millis_to_nanos, price_from_decimal, quantity_from_decimal},
    },
    http::models::{
        AssetCtx, Candle, ClearinghouseState, MarketTicker, PerpMeta, Position, Trade, TxflowOrder,
        UserFill,
    },
};

/// `BTC-USDC` becomes `BTC-USDC-PERP.TXFLOW`.
#[must_use]
pub fn instrument_id_from_name(name: &str) -> InstrumentId {
    InstrumentId::new(Symbol::new(format!("{name}-PERP")), *TXFLOW_VENUE)
}

/// Parses one `perpMeta` market.
///
/// # Errors
///
/// Returns an error if a step or the leverage is not positive, or a name is empty.
pub fn parse_instrument(meta: &PerpMeta, ts_init: UnixNanos) -> anyhow::Result<InstrumentAny> {
    anyhow::ensure!(
        meta.price_tick > Decimal::ZERO
            && meta.base_precision > Decimal::ZERO
            && meta.max_leverage > Decimal::ZERO
            // Symbols and currency codes panic on empty, blank or non-ASCII text
            && [&meta.name, &meta.base_currency, &meta.quote_currency]
                .iter()
                .all(|text| !text.is_empty() && text.bytes().all(|b| b.is_ascii_graphic())),
        "invalid configuration for market {:?}",
        meta.name
    );
    let price_precision = u8::try_from(meta.price_tick.normalize().scale())?;
    // A size step of 10 has scale 0: whole units, increment 10.
    let size_precision = u8::try_from(meta.base_precision.normalize().scale())?;
    let quote_currency = Currency::get_or_create_crypto(&meta.quote_currency);
    let margin_init = Decimal::ONE / meta.max_leverage;

    let instrument = CryptoPerpetual::builder()
        .instrument_id(instrument_id_from_name(&meta.name))
        .raw_symbol(Symbol::new(&meta.name))
        .base_currency(Currency::get_or_create_crypto(&meta.base_currency))
        .quote_currency(quote_currency)
        .settlement_currency(quote_currency)
        .is_inverse(false)
        .price_precision(price_precision)
        .size_precision(size_precision)
        .price_increment(price_from_decimal(meta.price_tick, price_precision)?)
        .size_increment(quantity_from_decimal(meta.base_precision, size_precision)?)
        .margin_init(margin_init)
        // assumption: the venue does not publish the maintenance margin; half of the initial
        // margin at maximum leverage is the Hyperliquid rule.
        .margin_maint(margin_init / Decimal::TWO)
        .ts_event(ts_init)
        .ts_init(ts_init)
        .build()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(InstrumentAny::CryptoPerpetual(instrument))
}

/// Trading status of a `perpMeta` market: delisted wins over halted.
#[must_use]
pub const fn market_status(meta: &PerpMeta) -> MarketStatusAction {
    if meta.delisted {
        MarketStatusAction::NotAvailableForTrading
    } else if meta.halt_trading {
        MarketStatusAction::Halt
    } else {
        MarketStatusAction::Trading
    }
}

/// # Errors
///
/// Returns an error if the price or size does not fit the instrument, the size is zero, or
/// the side is unknown.
pub fn parse_trade_tick(
    trade: &Trade,
    instrument: &InstrumentAny,
    ts_init: UnixNanos,
) -> anyhow::Result<TradeTick> {
    anyhow::ensure!(!trade.tid.is_empty(), "trade without an id");
    TradeTick::new_checked(
        instrument.id(),
        price_from_decimal(trade.px, instrument.price_precision())?,
        quantity_from_decimal(trade.sz, instrument.size_precision())?,
        parse_aggressor_side(&trade.side)?,
        TradeId::new_checked(&trade.tid)?,
        millis_to_nanos(trade.time),
        ts_init,
    )
}

/// The bar is stamped at its close: `T` is the last millisecond of the interval.
///
/// # Errors
///
/// Returns an error if a value does not fit the instrument or the bar is inconsistent.
pub fn parse_bar(
    candle: &Candle,
    bar_type: BarType,
    instrument: &InstrumentAny,
    ts_init: UnixNanos,
) -> anyhow::Result<Bar> {
    let price = |value| price_from_decimal(value, instrument.price_precision());
    Bar::new_checked(
        bar_type,
        price(candle.o)?,
        price(candle.h)?,
        price(candle.l)?,
        price(candle.c)?,
        quantity_from_decimal(candle.v, instrument.size_precision())?,
        millis_to_nanos(candle.close_time.saturating_add(1)),
        ts_init,
    )
}

/// The venue publishes funding in percent: `0.01` is 0.01 % per hour.
#[must_use]
pub fn funding_rate_update(
    instrument_id: InstrumentId,
    percent: Decimal,
    next_funding_ms: Option<u64>,
    ts_event: UnixNanos,
    ts_init: UnixNanos,
) -> FundingRateUpdate {
    FundingRateUpdate::new(
        instrument_id,
        percent / Decimal::ONE_HUNDRED,
        Some(FUNDING_INTERVAL_MINS),
        next_funding_ms.map(millis_to_nanos),
        ts_event,
        ts_init,
    )
}

#[must_use]
pub fn parse_ticker_funding(
    ticker: &MarketTicker,
    instrument_id: InstrumentId,
    ts_init: UnixNanos,
) -> FundingRateUpdate {
    let ts_event = ticker.block_time.map_or(ts_init, millis_to_nanos);
    funding_rate_update(
        instrument_id,
        ticker.funding_rate,
        ticker.next_funding_time,
        ts_event,
        ts_init,
    )
}

/// Mark price, index price (the oracle price) and funding rate of one `activeAssetCtx` frame.
/// A field the venue left out gives `None`.
///
/// # Errors
///
/// Returns an error if a price does not fit the instrument.
pub fn parse_asset_ctx(
    ctx: &AssetCtx,
    instrument: &InstrumentAny,
    ts_init: UnixNanos,
) -> anyhow::Result<(
    Option<MarkPriceUpdate>,
    Option<IndexPriceUpdate>,
    Option<FundingRateUpdate>,
)> {
    let id = instrument.id();
    let ts_event = ctx.block_time.map_or(ts_init, millis_to_nanos);
    let price = |value| price_from_decimal(value, instrument.price_precision());
    let mark = match ctx.mark_px {
        Some(px) => Some(MarkPriceUpdate::new(id, price(px)?, ts_event, ts_init)),
        None => None,
    };
    let index = match ctx.oracle_px {
        Some(px) => Some(IndexPriceUpdate::new(id, price(px)?, ts_event, ts_init)),
        None => None,
    };
    let funding = ctx
        .funding
        .map(|percent| funding_rate_update(id, percent, ctx.next_funding_time, ts_event, ts_init));
    Ok((mark, index, funding))
}

/// Trade id of a fill: the order id and the filled size of the order after this fill.
///
/// `userFills` has a venue trade id (`tid`), but no live channel carries it with the order id:
/// live fills come from `orderUpdates` rows, which have none. So the id is built from what
/// both sources agree on. shortcut: a `userFills` window that misses the first fills of an
/// order gives other ids than the live stream did, upgrade when the venue pushes fills.
///
/// # Errors
///
/// Returns an error if the id is longer than a trade id may be (36 characters).
pub fn fill_trade_id(oid: u64, cumulative: Decimal) -> anyhow::Result<TradeId> {
    Ok(TradeId::new_checked(format!(
        "{oid}-{}",
        cumulative.normalize()
    ))?)
}

/// Parses an order of `frontendOpenOrders`, `triggerOrders`, `historicalOrders`,
/// `orderDetails` or `orderUpdates`. `status` is the row status, or the `state` of an open
/// order.
///
/// # Errors
///
/// Returns an error if the side or the status is unknown, or a value does not fit the
/// instrument.
pub fn parse_order_status_report(
    order: &TxflowOrder,
    status: &str,
    status_ms: Option<u64>,
    account_id: AccountId,
    instrument: &InstrumentAny,
    ts_init: UnixNanos,
) -> anyhow::Result<OrderStatusReport> {
    let price_precision = instrument.price_precision();
    let size_precision = instrument.size_precision();
    let quantity = order.quantity();
    let mut order_status = parse_order_status(status)?;
    // `historicalOrders` leaves `sz` unreduced on a filled order: the status wins
    let filled = if order_status == OrderStatus::Filled {
        quantity
    } else {
        order.filled()
    };
    if order_status == OrderStatus::Accepted && filled > Decimal::ZERO {
        order_status = OrderStatus::PartiallyFilled;
    }

    let is_market = order
        .order_type
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("market"));
    // A trigger row does not say take-profit or stop-loss: both are reported as stops
    let order_type = match (order.is_trigger, is_market) {
        (true, true) => OrderType::StopMarket,
        (true, false) => OrderType::StopLimit,
        (false, true) => OrderType::Market,
        (false, false) => OrderType::Limit,
    };
    let (time_in_force, post_only) = match order.tif.as_deref() {
        Some(tif) => parse_time_in_force(tif),
        None if is_market && !order.is_trigger => (TimeInForce::Ioc, false),
        None => (TimeInForce::Gtc, false),
    };
    let ts_accepted = order.timestamp.map_or(ts_init, millis_to_nanos);
    let ts_last = status_ms.map_or(ts_accepted, millis_to_nanos);

    let mut report = OrderStatusReport::new(
        account_id,
        instrument.id(),
        order
            .cloid
            .as_deref()
            .and_then(|cloid| ClientOrderId::new_checked(cloid).ok()),
        VenueOrderId::new(order.oid.to_string()),
        Some(parse_order_side(&order.side)?),
        order_type,
        time_in_force,
        order_status,
        quantity_from_decimal(quantity, size_precision)?,
        quantity_from_decimal(filled, size_precision)?,
        ts_accepted,
        ts_last,
        ts_init,
        None,
    )
    .with_post_only(post_only)
    .with_reduce_only(order.reduce_only);

    if let Some(price) = order.limit_px.filter(|px| *px > Decimal::ZERO) {
        report = report.with_price(price_from_decimal(price, price_precision)?);
    }
    if let Some(avg_px) = order.avg_px() {
        report = report.with_avg_px(avg_px);
    }
    if let Some(trigger_px) = order.trigger_px.filter(|px| *px > Decimal::ZERO) {
        // Trigger orders fire on the mark price
        report = report
            .with_trigger_price(price_from_decimal(trigger_px, price_precision)?)
            .with_trigger_type(TriggerType::MarkPrice);
    }
    Ok(report)
}

/// Parses the fills of `/info userFills` (any order) into reports, oldest first.
/// A fill of an unknown market or with a value that does not fit is logged and skipped.
#[must_use]
pub fn parse_fill_reports(
    mut fills: Vec<UserFill>,
    account_id: AccountId,
    lookup: impl Fn(&str) -> Option<InstrumentAny>,
    ts_init: UnixNanos,
) -> Vec<FillReport> {
    fills.sort_by_key(|fill| fill.time);
    let mut cumulative: HashMap<u64, Decimal> = HashMap::new();
    let mut reports = Vec::with_capacity(fills.len());
    for fill in &fills {
        let total = cumulative.entry(fill.oid).or_default();
        *total = total.saturating_add(fill.sz);
        let Some(instrument) = lookup(&fill.instrument_key()) else {
            log::warn!("Skipping a fill of unknown market {}", fill.coin);
            continue;
        };
        match parse_fill_report(fill, *total, account_id, &instrument, ts_init) {
            Ok(report) => reports.push(report),
            Err(e) => log::warn!("Skipping a fill of order {}: {e}", fill.oid),
        }
    }
    reports
}

/// Parses one fill of `/info userFills`. `cumulative` is the filled size of its order after
/// this fill.
///
/// # Errors
///
/// Returns an error if the side is unknown or a value does not fit the instrument.
pub fn parse_fill_report(
    fill: &UserFill,
    cumulative: Decimal,
    account_id: AccountId,
    instrument: &InstrumentAny,
    ts_init: UnixNanos,
) -> anyhow::Result<FillReport> {
    // shortcut: the fee is booked in the quote currency whatever `feeToken` says, upgrade
    // when the venue charges perp fees in another token.
    let commission =
        Money::from_decimal(fill.fee.unwrap_or_default(), instrument.quote_currency())?;
    Ok(FillReport::new(
        account_id,
        instrument.id(),
        VenueOrderId::new(fill.oid.to_string()),
        fill_trade_id(fill.oid, cumulative)?,
        parse_order_side(&fill.side)?,
        quantity_from_decimal(fill.sz, instrument.size_precision())?,
        price_from_decimal(fill.px, instrument.price_precision())?,
        commission,
        parse_liquidity_side(fill.trade_side.as_deref().unwrap_or_default()),
        None,
        None,
        millis_to_nanos(fill.time),
        ts_init,
        None,
    ))
}

/// Parses a position of `clearinghouseState`. The sign of `szi` is the side.
///
/// # Errors
///
/// Returns an error if the size does not fit the instrument.
pub fn parse_position_status_report(
    position: &Position,
    account_id: AccountId,
    instrument: &InstrumentAny,
    ts_init: UnixNanos,
) -> anyhow::Result<PositionStatusReport> {
    let size = position.szi.unwrap_or_default();
    let quantity = quantity_from_decimal(size.abs(), instrument.size_precision())?;
    let position_side = if quantity.is_zero() {
        PositionSide::Flat
    } else if size > Decimal::ZERO {
        PositionSide::Long
    } else {
        PositionSide::Short
    };
    Ok(PositionStatusReport::new(
        account_id,
        instrument.id(),
        position_side,
        quantity,
        ts_init,
        ts_init,
        None,
        None,
        position.entry_px.filter(|px| *px > Decimal::ZERO),
    ))
}

/// The USDC balance of an account: total is the account value, locked is the margin in use.
///
/// # Errors
///
/// Returns an error if a value does not fit a USDC amount.
pub fn parse_account_balance(state: &ClearinghouseState) -> anyhow::Result<AccountBalance> {
    // An account under water has no free balance; a balance cannot be negative
    let total = state
        .margin_summary
        .account_value
        .unwrap_or_default()
        .max(Decimal::ZERO);
    let locked = state
        .margin_summary
        .total_margin_used
        .unwrap_or_default()
        .clamp(Decimal::ZERO, total);
    Ok(AccountBalance::from_total_and_locked(
        total,
        locked,
        Currency::USDC(),
    )?)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(false, false, MarketStatusAction::Trading)]
    #[case(false, true, MarketStatusAction::Halt)]
    #[case(true, false, MarketStatusAction::NotAvailableForTrading)]
    #[case(true, true, MarketStatusAction::NotAvailableForTrading)]
    fn market_status_follows_the_flags(
        #[case] delisted: bool,
        #[case] halt_trading: bool,
        #[case] expected: MarketStatusAction,
    ) {
        let meta: PerpMeta = serde_json::from_value(serde_json::json!({
            "name": "BTC-USDC", "baseCurrency": "BTC", "quoteCurrency": "USDC",
            "basePrecision": "0.0001", "priceTick": "0.1", "maxLeverage": 75, "index": 1,
            "delisted": delisted, "haltTrading": halt_trading,
        }))
        .unwrap();
        assert_eq!(market_status(&meta), expected);
    }
}
