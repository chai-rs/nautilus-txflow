//! Turns order rows of the venue into order events.
//!
//! The venue has no live fills channel and the `/exchange` answer carries no order id, so the
//! `orderUpdates` rows are the only source of `OrderAccepted` and of fills. A fill is the
//! change of the filled size between two rows of one order; its price comes from the change
//! of the average fill price.

use std::{collections::HashMap, sync::Arc};

use nautilus_core::UnixNanos;
use nautilus_live::ExecutionEventEmitter;
use nautilus_model::{
    enums::{LiquiditySide, OrderStatus, OrderType, TimeInForce},
    identifiers::{AccountId, ClientOrderId, VenueOrderId},
    instruments::{Instrument, InstrumentAny},
    orders::{Order, OrderAny},
    types::Money,
};
use parking_lot::Mutex;
use rust_decimal::Decimal;

use crate::{
    common::{
        enums::parse_order_status,
        instruments::InstrumentCache,
        parse::{millis_to_nanos, price_from_decimal, quantity_from_decimal},
    },
    http::{
        models::OrderRow,
        parse::{fill_trade_id, parse_order_status_report},
    },
};

const MAX_TRACKED_ORDERS: usize = 4096;

#[derive(Debug)]
struct Tracked {
    order: OrderAny,
    venue_order_id: Option<VenueOrderId>,
    accepted: bool,
    closed: bool,
    /// Filled size, filled notional and fee already sent as fills.
    filled: Decimal,
    notional: Decimal,
    fee: Decimal,
}

#[derive(Debug, Default)]
struct State {
    orders: HashMap<ClientOrderId, Tracked>,
    by_venue: HashMap<u64, ClientOrderId>,
}

/// Orders submitted in this session and the events already sent for them.
#[derive(Debug)]
pub struct OrderDispatch {
    account_id: AccountId,
    emitter: ExecutionEventEmitter,
    instruments: Arc<InstrumentCache>,
    state: Mutex<State>,
}

impl OrderDispatch {
    #[must_use]
    pub fn new(
        account_id: AccountId,
        emitter: ExecutionEventEmitter,
        instruments: Arc<InstrumentCache>,
    ) -> Self {
        Self {
            account_id,
            emitter,
            instruments,
            state: Mutex::default(),
        }
    }

    /// Starts tracking a submitted order. Tracking it twice changes nothing.
    pub fn track(&self, order: &OrderAny) {
        let mut state = self.state.lock();
        if state.orders.len() >= MAX_TRACKED_ORDERS {
            // shortcut: closed orders stay tracked until the map is full, then all of them
            // go; a row that arrives later still reaches the engine, as an order report.
            state.orders.retain(|_, tracked| !tracked.closed);
            let State { orders, by_venue } = &mut *state;
            by_venue.retain(|_, key| orders.contains_key(key));
        }
        state
            .orders
            .entry(order.client_order_id())
            .or_insert_with(|| Tracked {
                order: order.clone(),
                venue_order_id: None,
                accepted: false,
                closed: false,
                filled: Decimal::ZERO,
                notional: Decimal::ZERO,
                fee: Decimal::ZERO,
            });
    }

    /// The venue order id of a tracked order, once a row showed it.
    #[must_use]
    pub fn venue_order_id(&self, client_order_id: &ClientOrderId) -> Option<VenueOrderId> {
        self.state
            .lock()
            .orders
            .get(client_order_id)?
            .venue_order_id
    }

    /// Emits `OrderAccepted` once. For an order the venue does not show in `orderUpdates`.
    pub fn accepted(&self, client_order_id: &ClientOrderId, oid: u64, ts_event: UnixNanos) {
        let mut state = self.state.lock();
        let State { orders, by_venue } = &mut *state;
        let Some(tracked) = orders.get_mut(client_order_id) else {
            return;
        };
        if tracked.accepted || tracked.closed {
            return;
        }
        let venue_order_id = VenueOrderId::new(oid.to_string());
        tracked.venue_order_id = Some(venue_order_id);
        tracked.accepted = true;
        by_venue.insert(oid, *client_order_id);
        self.emitter
            .emit_order_accepted(&tracked.order, venue_order_id, ts_event);
    }

    /// Emits `OrderRejected` for an order the venue refused, unless a row already settled it.
    pub fn rejected(
        &self,
        client_order_id: &ClientOrderId,
        reason: &str,
        ts_event: UnixNanos,
        due_post_only: bool,
    ) {
        let mut state = self.state.lock();
        let Some(tracked) = state.orders.get_mut(client_order_id) else {
            return;
        };
        if tracked.accepted || tracked.closed {
            log::warn!("Venue refused order {client_order_id} after it was settled: {reason}");
            return;
        }
        tracked.closed = true;
        self.emitter
            .emit_order_rejected(&tracked.order, reason, ts_event, due_post_only);
    }

    /// Emits `OrderCanceled` once. For an order the venue does not show in `orderUpdates`.
    pub fn canceled(&self, client_order_id: &ClientOrderId, ts_event: UnixNanos) {
        let mut state = self.state.lock();
        let Some(tracked) = state.orders.get_mut(client_order_id) else {
            return;
        };
        if tracked.closed {
            return;
        }
        tracked.closed = true;
        self.emitter
            .emit_order_canceled(&tracked.order, tracked.venue_order_id, ts_event);
    }

    /// Handles one row of `orderUpdates`, `historicalOrders` or `orderDetails`.
    ///
    /// A row of a tracked order becomes events; the same row twice gives them once. A row of
    /// any other order becomes an order status report, which the engine reconciles.
    pub fn handle_row(&self, row: &OrderRow, ts_init: UnixNanos) {
        let order = &row.order;
        let Some(instrument) = self.instruments.lookup(&order.instrument_key()) else {
            log::warn!("Order {} is on unknown market {}", order.oid, order.coin);
            return;
        };
        let mut state = self.state.lock();
        let State { orders, by_venue } = &mut *state;
        let key = order
            .cloid
            .as_deref()
            .and_then(|cloid| ClientOrderId::new_checked(cloid).ok())
            .filter(|key| orders.contains_key(key))
            .or_else(|| by_venue.get(&order.oid).copied());
        let Some(tracked) = key.and_then(|key| orders.get_mut(&key)) else {
            drop(state);
            self.report_untracked(row, &instrument, ts_init);
            return;
        };
        if tracked.closed {
            return;
        }
        let status = match parse_order_status(&row.status) {
            Ok(status) => status,
            Err(e) => {
                log::warn!("Ignoring a row of order {}: {e}", order.oid);
                return;
            }
        };
        let client_order_id = tracked.order.client_order_id();
        let venue_order_id = VenueOrderId::new(order.oid.to_string());
        if tracked.venue_order_id != Some(venue_order_id) {
            tracked.venue_order_id = Some(venue_order_id);
            by_venue.insert(order.oid, client_order_id);
        }
        let ts_event = row
            .status_timestamp
            .or(order.timestamp)
            .map_or(ts_init, millis_to_nanos);

        if status == OrderStatus::Rejected && !tracked.accepted {
            tracked.closed = true;
            // The row gives no reason; the `/exchange` answer, if it has one, is logged
            self.emitter.emit_order_rejected(
                &tracked.order,
                "rejected by TxFlow (the order stream gives no reason)",
                ts_event,
                false,
            );
            return;
        }
        if !tracked.accepted {
            tracked.accepted = true;
            self.emitter
                .emit_order_accepted(&tracked.order, venue_order_id, ts_event);
        }

        if let Err(e) = self.emit_fill(tracked, row, status, &instrument, ts_event) {
            log::error!("No fill event for order {client_order_id}: {e}");
        }

        match status {
            OrderStatus::Filled => tracked.closed = true,
            OrderStatus::Canceled | OrderStatus::Rejected => {
                tracked.closed = true;
                self.emitter
                    .emit_order_canceled(&tracked.order, Some(venue_order_id), ts_event);
            }
            OrderStatus::Expired => {
                tracked.closed = true;
                self.emitter
                    .emit_order_expired(&tracked.order, Some(venue_order_id), ts_event);
            }
            _ => {}
        }
    }

    /// Emits the fill between the last row and this one, if there is one.
    fn emit_fill(
        &self,
        tracked: &mut Tracked,
        row: &OrderRow,
        status: OrderStatus,
        instrument: &InstrumentAny,
        ts_event: UnixNanos,
    ) -> anyhow::Result<()> {
        let order = &row.order;
        let cumulative = if status == OrderStatus::Filled {
            // `historicalOrders` leaves `sz` unreduced on a filled order: the status wins
            Some(order.quantity())
                .filter(|quantity| !quantity.is_zero())
                .unwrap_or_else(|| tracked.order.quantity().as_decimal())
        } else {
            order.filled()
        };
        let overflow = || anyhow::anyhow!("the fill amounts of the row overflow");
        let delta = cumulative
            .checked_sub(tracked.filled)
            .ok_or_else(overflow)?;
        // A canceled row may show a remaining size of zero: without a fill status or an
        // average price that is not a fill
        let shows_fill = matches!(status, OrderStatus::Filled | OrderStatus::PartiallyFilled)
            || row.status.to_ascii_lowercase().starts_with("partial")
            || order.avg_px().is_some();
        if delta <= Decimal::ZERO || !shows_fill {
            return Ok(());
        }

        let (last_px, notional) = match order.avg_px() {
            Some(avg_px) => {
                let notional = avg_px.checked_mul(cumulative).ok_or_else(overflow)?;
                // Rounded averages can give a price off the book: the average is closer
                let px = notional
                    .checked_sub(tracked.notional)
                    .and_then(|change| change.checked_div(delta))
                    .filter(|px| *px > Decimal::ZERO)
                    .unwrap_or(avg_px);
                (px, notional)
            }
            None => {
                // shortcut: without an average fill price the limit price stands in, upgrade
                // when the venue sends fills.
                let px = order
                    .limit_px
                    .filter(|px| *px > Decimal::ZERO)
                    .or_else(|| tracked.order.price().map(|px| px.as_decimal()))
                    .ok_or_else(|| anyhow::anyhow!("the row has no fill price"))?;
                let notional = px
                    .checked_mul(delta)
                    .and_then(|change| tracked.notional.checked_add(change))
                    .ok_or_else(overflow)?;
                (px, notional)
            }
        };
        let quote_currency = instrument.quote_currency();
        // `fee` is the fee of the order so far (assumed: the docs say "charged so far")
        let commission = order
            .fee
            .map(|fee| fee.checked_sub(tracked.fee).ok_or_else(overflow))
            .transpose()?
            .map(|change| Money::from_decimal(change, quote_currency))
            .transpose()?;
        let trade_id = fill_trade_id(order.oid, cumulative)?;
        let last_qty = quantity_from_decimal(delta, instrument.size_precision())?;
        let last_px = price_from_decimal(last_px, instrument.price_precision())?;
        // A post-only order can only make, a market or IOC order can only take
        let liquidity_side = if tracked.order.is_post_only() {
            LiquiditySide::Maker
        } else if tracked.order.order_type() == OrderType::Market
            || tracked.order.time_in_force() == TimeInForce::Ioc
        {
            LiquiditySide::Taker
        } else {
            LiquiditySide::NoLiquiditySide
        };

        tracked.filled = cumulative;
        tracked.notional = notional;
        tracked.fee = order.fee.unwrap_or(tracked.fee);
        self.emitter.emit_order_filled(
            &tracked.order,
            VenueOrderId::new(order.oid.to_string()),
            None,
            trade_id,
            last_qty,
            last_px,
            quote_currency,
            commission,
            liquidity_side,
            ts_event,
        );
        Ok(())
    }

    fn report_untracked(&self, row: &OrderRow, instrument: &InstrumentAny, ts_init: UnixNanos) {
        match parse_order_status_report(
            &row.order,
            &row.status,
            row.status_timestamp,
            self.account_id,
            instrument,
            ts_init,
        ) {
            // A refused order of another session never existed for the engine
            Ok(report) if report.order_status == OrderStatus::Rejected => {}
            Ok(report) => self.emitter.send_order_status_report(report),
            Err(e) => log::warn!("Ignoring a row of order {}: {e}", row.order.oid),
        }
    }
}
