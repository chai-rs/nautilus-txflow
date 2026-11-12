//! Order book stream: `/ws l2BookDepth` frames to Nautilus deltas.
//!
//! The channel sends a full snapshot (200 levels per side, `prevSeq` null) and then deltas
//! whose `prevSeq` is the `seq` of the frame before. Full snapshots come again every few
//! seconds. A delta that does not follow the last frame is a gap: the book is dropped until
//! the next snapshot.
//!
//! The levels of the applied frames are kept, so the best ten per side can be cut from them
//! as a depth snapshot.

use std::collections::BTreeMap;

use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{BookOrder, DEPTH10_LEN, OrderBookDelta, OrderBookDeltas, OrderBookDepth},
    enums::{BookAction, OrderSide, RecordFlag},
    instruments::{Instrument, InstrumentAny},
};

use rust_decimal::Decimal;

use crate::{
    common::parse::{millis_to_nanos, price_from_decimal, quantity_from_decimal},
    http::models::{Level, Levels},
    websocket::messages::BookDepth,
};

#[derive(Debug, thiserror::Error)]
pub enum BookError {
    /// Reported once per gap. The caller should subscribe to the channel again.
    #[error("book sequence gap: expected prevSeq {expected}, got {got:?}")]
    Gap { expected: u64, got: Option<u64> },
    #[error("invalid book frame: {0}")]
    Invalid(#[from] anyhow::Error),
}

/// Sequence state and levels of one market.
#[derive(Debug, Default)]
pub struct BookSync {
    /// `seq` of the last applied frame. `None` while waiting for a snapshot.
    seq: Option<u64>,
    /// Levels by price, bids then asks. Valid while `seq` is set.
    levels: [BTreeMap<Decimal, Level>; 2],
}

impl BookSync {
    /// Converts one frame. Returns `None` for a delta that arrives before a snapshot and for a
    /// delta without levels.
    ///
    /// # Errors
    ///
    /// Returns [`BookError::Gap`] when a delta does not follow the last frame, and
    /// [`BookError::Invalid`] when a level does not fit the instrument.
    pub fn apply(
        &mut self,
        frame: &BookDepth,
        instrument: &InstrumentAny,
        ts_init: UnixNanos,
    ) -> Result<Option<OrderBookDeltas>, BookError> {
        let ts_event = millis_to_nanos(frame.time);
        if frame.kind == "snapshot" {
            let deltas = snapshot_deltas(instrument, &frame.levels, frame.seq, ts_event, ts_init)?;
            self.seq = Some(frame.seq);
            self.levels = Default::default();
            self.keep(&frame.levels);
            return Ok(Some(deltas));
        }

        let Some(expected) = self.seq else {
            return Ok(None);
        };
        if frame.prev_seq != Some(expected) {
            self.seq = None;
            return Err(BookError::Gap {
                expected,
                got: frame.prev_seq,
            });
        }

        let mut deltas = Vec::with_capacity(frame.levels.0.len() + frame.levels.1.len());
        for (side, levels) in sides(&frame.levels) {
            for level in levels {
                let action = if level.sz.is_zero() {
                    BookAction::Delete
                } else {
                    BookAction::Update
                };
                let order = order(instrument, side, level);
                let delta = order.and_then(|order| {
                    OrderBookDelta::new_checked(
                        instrument.id(),
                        action,
                        order,
                        0,
                        frame.seq,
                        ts_event,
                        ts_init,
                    )
                });
                match delta {
                    Ok(delta) => deltas.push(delta),
                    Err(e) => {
                        // A half-applied delta would leave a wrong book
                        self.seq = None;
                        return Err(BookError::Invalid(e));
                    }
                }
            }
        }
        self.seq = Some(frame.seq);
        self.keep(&frame.levels);
        Ok(finish(instrument, deltas)?)
    }

    /// Size zero removes a level.
    fn keep(&mut self, levels: &Levels) {
        for (book, (_, levels)) in self.levels.iter_mut().zip(sides(levels)) {
            for level in levels {
                if level.sz.is_zero() {
                    book.remove(&level.px);
                } else {
                    book.insert(level.px, level.clone());
                }
            }
        }
    }

    /// The best ten levels per side after the last applied frame, with the order count of
    /// each level.
    ///
    /// # Errors
    ///
    /// Returns an error if a level does not fit the instrument.
    pub fn depth(
        &self,
        instrument: &InstrumentAny,
        sequence: u64,
        ts_event: UnixNanos,
        ts_init: UnixNanos,
    ) -> anyhow::Result<OrderBookDepth> {
        let [bids, asks] = &self.levels;
        let side = |side, levels: &mut dyn Iterator<Item = &Level>| {
            levels
                .take(DEPTH10_LEN)
                .map(|level| Ok((order(instrument, side, level)?, level.n)))
                .collect::<anyhow::Result<(Vec<_>, Vec<_>)>>()
        };
        let (bids, bid_counts) = side(OrderSide::Buy, &mut bids.values().rev())?;
        let (asks, ask_counts) = side(OrderSide::Sell, &mut asks.values())?;
        OrderBookDepth::new_checked(
            instrument.id(),
            bids,
            asks,
            bid_counts,
            ask_counts,
            RecordFlag::F_SNAPSHOT as u8,
            sequence,
            ts_event,
            ts_init,
        )
    }
}

/// Deltas that replace the whole book: a clear, then one add per level.
///
/// # Errors
///
/// Returns an error if a level does not fit the instrument.
pub fn snapshot_deltas(
    instrument: &InstrumentAny,
    levels: &Levels,
    sequence: u64,
    ts_event: UnixNanos,
    ts_init: UnixNanos,
) -> anyhow::Result<OrderBookDeltas> {
    let id = instrument.id();
    let flags = RecordFlag::F_SNAPSHOT as u8;
    let mut deltas = vec![OrderBookDelta::clear(id, sequence, ts_event, ts_init)];
    for (side, levels) in sides(levels) {
        for level in levels.iter().filter(|level| !level.sz.is_zero()) {
            deltas.push(OrderBookDelta::new_checked(
                id,
                BookAction::Add,
                order(instrument, side, level)?,
                flags,
                sequence,
                ts_event,
                ts_init,
            )?);
        }
    }
    finish(instrument, deltas)?.ok_or_else(|| anyhow::anyhow!("snapshot without deltas"))
}

fn sides(levels: &Levels) -> [(OrderSide, &Vec<Level>); 2] {
    [(OrderSide::Buy, &levels.0), (OrderSide::Sell, &levels.1)]
}

fn order(instrument: &InstrumentAny, side: OrderSide, level: &Level) -> anyhow::Result<BookOrder> {
    // Order id 0: an L2_MBP book keys its levels by price
    Ok(BookOrder::new(
        side,
        price_from_decimal(level.px, instrument.price_precision())?,
        quantity_from_decimal(level.sz, instrument.size_precision())?,
        0,
    ))
}

fn finish(
    instrument: &InstrumentAny,
    mut deltas: Vec<OrderBookDelta>,
) -> anyhow::Result<Option<OrderBookDeltas>> {
    let Some(last) = deltas.last_mut() else {
        return Ok(None);
    };
    last.flags |= RecordFlag::F_LAST as u8;
    Ok(Some(OrderBookDeltas::new_checked(instrument.id(), deltas)?))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::http::{models::PerpMeta, parse::parse_instrument};

    fn instrument() -> InstrumentAny {
        let meta: PerpMeta = serde_json::from_str(
            r#"{"name":"BTC-USDC","baseCurrency":"BTC","quoteCurrency":"USDC","basePrecision":"0.0001","priceTick":"0.1","maxLeverage":75,"index":1}"#,
        )
        .unwrap();
        parse_instrument(&meta, UnixNanos::default()).unwrap()
    }

    fn frame(json: &str) -> BookDepth {
        serde_json::from_str(json).unwrap()
    }

    const SNAPSHOT: &str = r#"{"coin":"BTC-USDC","id":1,"time":1794336500000,"seq":10,"prevSeq":null,"type":"snapshot","levels":[[{"px":"73381.2","sz":"0.6110","n":2},{"px":"73381.0","sz":"1.0000","n":1}],[{"px":"73393.8","sz":"0.2000","n":1}]]}"#;
    const DELTA: &str = r#"{"coin":"BTC-USDC","id":1,"time":1794336500100,"seq":11,"prevSeq":10,"type":"delta","levels":[[{"px":"73381.2","sz":"0.5000","n":1}],[{"px":"73393.8","sz":"0.0000","n":0}]]}"#;

    #[rstest]
    fn snapshot_then_delta() {
        let instrument = instrument();
        let mut sync = BookSync::default();
        let ts = UnixNanos::from(5);

        let snapshot = sync
            .apply(&frame(SNAPSHOT), &instrument, ts)
            .unwrap()
            .unwrap();
        let actions: Vec<_> = snapshot.deltas.iter().map(|d| d.action).collect();
        assert_eq!(
            actions,
            [
                BookAction::Clear,
                BookAction::Add,
                BookAction::Add,
                BookAction::Add
            ]
        );
        assert_eq!(
            snapshot.deltas[3].flags,
            RecordFlag::F_SNAPSHOT as u8 | RecordFlag::F_LAST as u8
        );
        assert_eq!(snapshot.sequence, 10);
        assert_eq!(
            snapshot.ts_event,
            UnixNanos::from(1_794_336_500_000_000_000)
        );

        let delta = sync.apply(&frame(DELTA), &instrument, ts).unwrap().unwrap();
        assert_eq!(delta.deltas[0].action, BookAction::Update);
        assert_eq!(delta.deltas[0].order.side, Some(OrderSide::Buy));
        assert_eq!(delta.deltas[1].action, BookAction::Delete);
        assert_eq!(delta.deltas[1].order.side, Some(OrderSide::Sell));
        assert_eq!(delta.deltas[0].flags, 0);
        assert_eq!(delta.deltas[1].flags, RecordFlag::F_LAST as u8);
    }

    #[rstest]
    fn depth_is_cut_from_the_kept_levels() {
        let instrument = instrument();
        let mut sync = BookSync::default();
        let ts = UnixNanos::from(5);
        // Twelve bids from 100.0 down: only the best ten are in the depth
        let bids: Vec<String> = (0..12)
            .map(|i| format!(r#"{{"px":"{}.0","sz":"1.0000","n":{}}}"#, 100 - i, i + 1))
            .collect();
        let snapshot = format!(
            r#"{{"id":1,"time":1,"seq":10,"prevSeq":null,"type":"snapshot","levels":[[{}],[{{"px":"101.0","sz":"2.0000","n":3}}]]}}"#,
            bids.join(",")
        );
        sync.apply(&frame(&snapshot), &instrument, ts).unwrap();
        // The delta removes the best bid and adds a better ask
        let delta = r#"{"id":1,"time":2,"seq":11,"prevSeq":10,"type":"delta","levels":[[{"px":"100.0","sz":"0","n":0}],[{"px":"100.5","sz":"0.5000","n":1}]]}"#;
        sync.apply(&frame(delta), &instrument, ts).unwrap();

        let depth = sync.depth(&instrument, 11, ts, ts).unwrap();
        let prices = |orders: &[BookOrder]| -> Vec<String> {
            orders.iter().map(|order| order.price.to_string()).collect()
        };
        assert_eq!(depth.bids.len(), 10);
        assert_eq!(prices(&depth.bids)[..2], ["99.0", "98.0"]);
        assert_eq!(depth.bids[9].price.to_string(), "90.0");
        assert_eq!(depth.bid_counts[0], 2);
        assert_eq!(prices(&depth.asks), ["100.5", "101.0"]);
        assert_eq!(depth.ask_counts[..], [1, 3]);
        assert_eq!(depth.sequence, 11);
    }

    #[rstest]
    fn delta_before_snapshot_is_dropped() {
        let mut sync = BookSync::default();
        let result = sync.apply(&frame(DELTA), &instrument(), UnixNanos::default());
        assert!(matches!(result, Ok(None)));
    }

    #[rstest]
    fn gap_is_reported_once_and_a_snapshot_recovers() {
        let instrument = instrument();
        let mut sync = BookSync::default();
        let ts = UnixNanos::default();
        sync.apply(&frame(SNAPSHOT), &instrument, ts).unwrap();

        let late = DELTA.replace(r#""prevSeq":10"#, r#""prevSeq":12"#);
        let result = sync.apply(&frame(&late), &instrument, ts);
        assert!(matches!(
            result,
            Err(BookError::Gap {
                expected: 10,
                got: Some(12)
            })
        ));
        assert!(matches!(
            sync.apply(&frame(DELTA), &instrument, ts),
            Ok(None)
        ));

        assert!(
            sync.apply(&frame(SNAPSHOT), &instrument, ts)
                .unwrap()
                .is_some()
        );
        assert!(
            sync.apply(&frame(DELTA), &instrument, ts)
                .unwrap()
                .is_some()
        );
    }

    #[rstest]
    fn empty_snapshot_clears_the_book() {
        let empty = r#"{"coin":"BTC-USDC","id":1,"time":1,"seq":3,"prevSeq":null,"type":"snapshot","levels":[[],[]]}"#;
        let mut sync = BookSync::default();
        let deltas = sync
            .apply(&frame(empty), &instrument(), UnixNanos::default())
            .unwrap()
            .unwrap();
        assert_eq!(deltas.deltas.len(), 1);
        assert_eq!(deltas.deltas[0].action, BookAction::Clear);
    }
}
