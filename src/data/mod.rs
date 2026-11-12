//! Live market data client.
//!
//! The node stream (`/ws`) feeds books, quotes, trades, mark and index prices and funding
//! rates. The API-service stream (`/api-ws`) feeds candles. Both need no credentials.

use std::{future::Future, sync::Arc, time::Duration};

use ahash::AHashMap;
use anyhow::Context;
use dashmap::{DashMap, DashSet};
use nautilus_common::{
    cache::InstrumentLookupError,
    clients::{DataClient, execute_bar_subscriptions, execute_instrument_subscriptions},
    live::{get_runtime, runner::get_data_event_sender, sender::EventSender},
    messages::{
        DataEvent,
        data::{
            BarsResponse, BookResponse, DataResponse, FundingRatesResponse, InstrumentResponse,
            InstrumentsResponse, RequestBars, RequestBookSnapshot, RequestFundingRates,
            RequestInstrument, RequestInstruments, RequestTrades, SubscribeBars,
            SubscribeBookDeltas, SubscribeFundingRates, SubscribeIndexPrices, SubscribeInstrument,
            SubscribeInstruments, SubscribeMarkPrices, SubscribeQuotes, SubscribeTrades,
            TradesResponse, UnsubscribeBars, UnsubscribeBookDeltas, UnsubscribeFundingRates,
            UnsubscribeIndexPrices, UnsubscribeInstrument, UnsubscribeInstruments,
            UnsubscribeMarkPrices, UnsubscribeQuotes, UnsubscribeTrades,
        },
    },
};
use nautilus_core::{
    UnixNanos,
    datetime::datetime_to_unix_nanos,
    time::{AtomicTime, get_atomic_clock_realtime},
};
use nautilus_model::{
    data::{BarType, Data, QuoteTick},
    enums::BookType,
    identifiers::{ClientId, InstrumentId, Venue},
    instruments::{Instrument, InstrumentAny},
    orderbook::OrderBook,
};
use rust_decimal::Decimal;
use serde_json::json;
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};

use crate::{
    book::BookSync,
    common::{
        consts::TXFLOW_VENUE,
        enums::bar_interval,
        parse::{millis_to_nanos, price_from_decimal, quantity_from_decimal},
    },
    config::{data::TxflowDataClientConfig, ws_urls},
    http::{
        client::TxflowHttpClient,
        models::Candle,
        parse::{parse_asset_ctx, parse_bar, parse_trade_tick},
    },
    websocket::{
        TxflowWebSocketClient, TxflowWsError,
        messages::{Bbo, WsMessage},
    },
};

const BOOK_CHANNEL: &str = "l2BookDepth";
const CANDLE_CHANNEL: &str = "candle";
/// A candle is emitted as a bar this long after its interval ended, unless a later candle
/// arrived first.
const BAR_CLOSE_GRACE_MS: u64 = 2_000;

/// What a strategy subscribed to. Several kinds share one venue channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Deltas,
    Quotes,
    Trades,
    MarkPrices,
    IndexPrices,
    FundingRates,
}

impl Kind {
    const ALL: [Self; 6] = [
        Self::Deltas,
        Self::Quotes,
        Self::Trades,
        Self::MarkPrices,
        Self::IndexPrices,
        Self::FundingRates,
    ];

    const fn channel(self) -> &'static str {
        match self {
            Self::Deltas => BOOK_CHANNEL,
            Self::Quotes => "bbo",
            Self::Trades => "trades",
            Self::MarkPrices | Self::IndexPrices | Self::FundingRates => "activeAssetCtx",
        }
    }

    fn of(data: &Data) -> Option<(Self, InstrumentId)> {
        match data {
            Data::BookDeltas(deltas) => Some((Self::Deltas, deltas.instrument_id)),
            Data::Quote(quote) => Some((Self::Quotes, quote.instrument_id)),
            Data::Trade(trade) => Some((Self::Trades, trade.instrument_id)),
            Data::MarkPrice(mark) => Some((Self::MarkPrices, mark.instrument_id)),
            Data::IndexPrice(index) => Some((Self::IndexPrices, index.instrument_id)),
            Data::FundingRate(rate) => Some((Self::FundingRates, rate.instrument_id)),
            _ => None,
        }
    }
}

/// A `bbo` frame as a quote. `None` while one side of the book is empty.
///
/// # Errors
///
/// Returns an error if a price or size does not fit the instrument.
pub fn parse_quote(
    bbo: &Bbo,
    instrument: &InstrumentAny,
    ts_init: UnixNanos,
) -> anyhow::Result<Option<QuoteTick>> {
    let (Some(bid), Some(ask)) = (&bbo.bbo.0, &bbo.bbo.1) else {
        return Ok(None);
    };
    let price = |value| price_from_decimal(value, instrument.price_precision());
    let size = |value| quantity_from_decimal(value, instrument.size_precision());
    Ok(Some(QuoteTick::new_checked(
        instrument.id(),
        price(bid.px)?,
        price(ask.px)?,
        size(bid.sz)?,
        size(ask.sz)?,
        millis_to_nanos(bbo.time),
        ts_init,
    )?))
}

/// Venue key of a candle stream: instrument id (`perpMeta.index`) and interval.
type CandleKey = (String, String);

/// State of the stream task.
#[derive(Debug, Default)]
struct State {
    books: AHashMap<InstrumentId, BookSync>,
    /// Last funding rate sent, to send changes only: the venue repeats it in every frame.
    funding: AHashMap<InstrumentId, (Decimal, Option<UnixNanos>)>,
    /// Latest candle of each stream, and whether it went out as a bar.
    candles: AHashMap<CandleKey, (Candle, bool)>,
}

/// State shared by the client and its tasks.
#[derive(Debug, Clone)]
struct Feed {
    http: TxflowHttpClient,
    // `connect` and `disconnect` of the WebSocket client need `&mut self`
    node: Arc<tokio::sync::Mutex<TxflowWebSocketClient>>,
    api: Arc<tokio::sync::Mutex<TxflowWebSocketClient>>,
    subscriptions: Arc<DashSet<(Kind, InstrumentId)>>,
    bars: Arc<DashMap<CandleKey, BarType>>,
    sender: EventSender<DataEvent>,
    clock: &'static AtomicTime,
}

impl Feed {
    fn send(&self, data: Data) {
        if let Err(e) = self.sender.send(DataEvent::Data(data)) {
            log::error!("Failed to send TxFlow data: {e}");
        }
    }

    /// Sends `data` if a strategy subscribed to its kind.
    fn send_subscribed(&self, data: Data) {
        if Kind::of(&data).is_some_and(|key| self.subscriptions.contains(&key)) {
            self.send(data);
        }
    }

    fn wants_channel(&self, channel: &str, instrument_id: InstrumentId) -> bool {
        Kind::ALL.iter().any(|kind| {
            kind.channel() == channel && self.subscriptions.contains(&(*kind, instrument_id))
        })
    }

    async fn set_channel(
        &self,
        channel: &str,
        coin: &str,
        subscribe: bool,
    ) -> Result<(), TxflowWsError> {
        let subscription = json!({"type": channel, "coin": coin});
        let ws = self.node.lock().await;
        if subscribe {
            ws.subscribe(subscription).await
        } else {
            ws.unsubscribe(&subscription).await
        }
    }

    async fn set_candles(&self, key: &CandleKey, subscribe: bool) -> Result<(), TxflowWsError> {
        let subscription = json!({"type": CANDLE_CHANNEL, "coin": key.0, "interval": key.1});
        let ws = self.api.lock().await;
        if subscribe {
            ws.subscribe(subscription).await
        } else {
            ws.unsubscribe(&subscription).await
        }
    }

    /// Consumes both streams until one ends.
    async fn run(
        self,
        mut node_rx: UnboundedReceiver<WsMessage>,
        mut api_rx: UnboundedReceiver<WsMessage>,
    ) {
        let mut state = State::default();
        let mut timer = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                message = node_rx.recv() => match message {
                    Some(message) => self.on_node(message, &mut state).await,
                    None => break,
                },
                message = api_rx.recv() => match message {
                    Some(WsMessage::Candle(candle)) => self.on_candle(candle, &mut state),
                    Some(_) => {}
                    None => break,
                },
                _ = timer.tick() => self.close_bars(&mut state),
            }
        }
        log::debug!("TxFlow WebSocket stream ended");
    }

    async fn on_node(&self, message: WsMessage, state: &mut State) {
        let ts_init = self.clock.get_time_ns();
        let instruments = self.http.instruments();
        match message {
            // The venue sends a new snapshot for each subscription sent again
            WsMessage::Reconnected => state.books.clear(),
            WsMessage::BookDepth(frame) => {
                let Some(instrument) = instruments.lookup(&frame.id) else {
                    return;
                };
                let id = instrument.id();
                if !self.subscriptions.contains(&(Kind::Deltas, id)) {
                    state.books.remove(&id);
                    return;
                }
                let book = state.books.entry(id).or_default();
                match book.apply(&frame, &instrument, ts_init) {
                    Ok(Some(deltas)) => self.send(deltas.into()),
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("TxFlow book of {id} dropped, subscribing again: {e}");
                        // A subscribe is answered with a snapshot
                        for subscribe in [false, true] {
                            if let Err(e) =
                                self.set_channel(BOOK_CHANNEL, &frame.id, subscribe).await
                            {
                                log::error!("Failed to resubscribe TxFlow book of {id}: {e}");
                            }
                        }
                    }
                }
            }
            WsMessage::Bbo(bbo) => {
                let Some(instrument) = instruments.lookup(&bbo.coin) else {
                    return;
                };
                match parse_quote(&bbo, &instrument, ts_init) {
                    Ok(Some(quote)) => self.send_subscribed(Data::Quote(quote)),
                    Ok(None) => {}
                    Err(e) => log::warn!("TxFlow quote of {} skipped: {e}", instrument.id()),
                }
            }
            WsMessage::Trades(trades) => {
                for trade in trades {
                    let key = trade
                        .instrument_id
                        .map_or_else(|| trade.coin.clone(), |id| id.to_string());
                    let Some(instrument) = instruments.lookup(&key) else {
                        continue;
                    };
                    match parse_trade_tick(&trade, &instrument, ts_init) {
                        Ok(tick) => self.send_subscribed(Data::Trade(tick)),
                        Err(e) => log::warn!("TxFlow trade of {} skipped: {e}", instrument.id()),
                    }
                }
            }
            WsMessage::AssetCtx(frame) => {
                let Some(instrument) = instruments.lookup(&frame.coin) else {
                    return;
                };
                let id = instrument.id();
                let (mark, index, funding) = match parse_asset_ctx(&frame.ctx, &instrument, ts_init)
                {
                    Ok(updates) => updates,
                    Err(e) => {
                        log::warn!("TxFlow asset context of {id} skipped: {e}");
                        return;
                    }
                };
                if let Some(mark) = mark {
                    self.send_subscribed(Data::MarkPrice(mark));
                }
                if let Some(index) = index {
                    self.send_subscribed(Data::IndexPrice(index));
                }
                if !self.subscriptions.contains(&(Kind::FundingRates, id)) {
                    // A new subscriber gets the current rate first
                    state.funding.remove(&id);
                } else if let Some(funding) = funding {
                    let value = (funding.rate, funding.next_funding_ns);
                    if state.funding.insert(id, value) != Some(value) {
                        self.send(Data::FundingRate(funding));
                    }
                }
            }
            _ => {}
        }
    }

    /// Keeps the latest candle of a stream. A candle of a later interval closes the one before.
    fn on_candle(&self, candle: Candle, state: &mut State) {
        let key = (candle.s.clone(), candle.i.clone());
        if !self.bars.contains_key(&key) {
            state.candles.remove(&key);
            return;
        }
        match state.candles.get_mut(&key) {
            Some((last, _)) if candle.t < last.t => {}
            // An update after the bar went out is dropped: the bar cannot be sent twice
            Some((last, sent)) if candle.t == last.t => {
                if !*sent {
                    *last = candle;
                }
            }
            Some((last, sent)) => {
                if !*sent {
                    self.send_bar(&key, last);
                }
                (*last, *sent) = (candle, false);
            }
            None => {
                state.candles.insert(key, (candle, false));
            }
        }
    }

    /// Sends the candles whose interval ended: the venue sends nothing at the end of one.
    fn close_bars(&self, state: &mut State) {
        let now_ms = self.clock.get_time_ms();
        for (key, (candle, sent)) in &mut state.candles {
            if !*sent && candle.close_time.saturating_add(BAR_CLOSE_GRACE_MS) < now_ms {
                self.send_bar(key, candle);
                *sent = true;
            }
        }
    }

    fn send_bar(&self, key: &CandleKey, candle: &Candle) {
        let Some(bar_type) = self.bars.get(key).map(|bar_type| *bar_type) else {
            return;
        };
        let Some(instrument) = self.http.instruments().get(&bar_type.instrument_id()) else {
            return;
        };
        match parse_bar(candle, bar_type, &instrument, self.clock.get_time_ns()) {
            Ok(bar) => self.send(Data::Bar(bar)),
            Err(e) => log::warn!("TxFlow {bar_type} bar skipped: {e}"),
        }
    }

    /// Disconnects both sockets and forgets every subscription.
    async fn close(self) {
        for ws in [&self.node, &self.api] {
            let mut ws = ws.lock().await;
            ws.disconnect().await;
            ws.clear_subscriptions();
        }
        self.subscriptions.clear();
        self.bars.clear();
    }
}

/// Market data client for TxFlow.
#[derive(Debug)]
pub struct TxflowDataClient {
    client_id: ClientId,
    config: TxflowDataClientConfig,
    feed: Feed,
    feed_task: Option<JoinHandle<()>>,
    close_task: Option<JoinHandle<()>>,
    is_connected: bool,
}

impl TxflowDataClient {
    /// Creates a new [`TxflowDataClient`]. Nothing is connected until [`DataClient::connect`].
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new(client_id: ClientId, config: TxflowDataClientConfig) -> anyhow::Result<Self> {
        let http = TxflowHttpClient::new(
            config.environment,
            config.base_url_http.clone(),
            config.http_timeout_secs,
            config.proxy_url.clone(),
        )?;
        let (node_url, api_url) = ws_urls(config.environment, config.base_url_http.as_deref());
        let socket = |ws| Arc::new(tokio::sync::Mutex::new(ws));
        let feed = Feed {
            http,
            node: socket(TxflowWebSocketClient::node(node_url)),
            api: socket(TxflowWebSocketClient::api(api_url)),
            subscriptions: Arc::new(DashSet::new()),
            bars: Arc::new(DashMap::new()),
            sender: get_data_event_sender(),
            clock: get_atomic_clock_realtime(),
        };

        Ok(Self {
            client_id,
            config,
            feed,
            feed_task: None,
            close_task: None,
            is_connected: false,
        })
    }

    fn instrument(&self, instrument_id: &InstrumentId) -> anyhow::Result<InstrumentAny> {
        self.feed
            .http
            .instruments()
            .get(instrument_id)
            .ok_or_else(|| InstrumentLookupError::not_found(*instrument_id).into())
    }

    /// The venue's id of the instrument, as the streams take it.
    fn coin(&self, instrument_id: &InstrumentId) -> anyhow::Result<String> {
        self.feed
            .http
            .instruments()
            .asset(instrument_id)
            .map(|asset| asset.to_string())
            .ok_or_else(|| InstrumentLookupError::not_found(*instrument_id).into())
    }

    fn candle_key(&self, bar_type: &BarType) -> anyhow::Result<CandleKey> {
        let (interval, _) = bar_interval(bar_type)?;
        Ok((self.coin(&bar_type.instrument_id())?, interval.to_string()))
    }

    fn spawn_channel(&self, channel: &'static str, coin: String, subscribe: bool) {
        let feed = self.feed.clone();
        get_runtime().spawn(async move {
            if let Err(e) = feed.set_channel(channel, &coin, subscribe).await {
                log::error!(
                    "Failed to set TxFlow {channel} subscription of {coin} to {subscribe}: {e}"
                );
            }
        });
    }

    fn spawn_candles(&self, key: CandleKey, subscribe: bool) {
        let feed = self.feed.clone();
        get_runtime().spawn(async move {
            if let Err(e) = feed.set_candles(&key, subscribe).await {
                log::error!("Failed to set TxFlow candle subscription {key:?} to {subscribe}: {e}");
            }
        });
    }

    fn subscribe_kind(&self, kind: Kind, instrument_ids: &[InstrumentId]) -> anyhow::Result<()> {
        execute_instrument_subscriptions(instrument_ids, |instrument_id| {
            let coin = self.coin(&instrument_id)?;
            let channel = kind.channel();
            let first = !self.feed.wants_channel(channel, instrument_id);
            self.feed.subscriptions.insert((kind, instrument_id));

            if first {
                self.spawn_channel(channel, coin, true);
            }
            Ok(())
        })
    }

    fn unsubscribe_kind(&self, kind: Kind, instrument_ids: &[InstrumentId]) -> anyhow::Result<()> {
        execute_instrument_subscriptions(instrument_ids, |instrument_id| {
            let channel = kind.channel();
            if self
                .feed
                .subscriptions
                .remove(&(kind, instrument_id))
                .is_none()
                || self.feed.wants_channel(channel, instrument_id)
            {
                return Ok(());
            }

            self.spawn_channel(channel, self.coin(&instrument_id)?, false);
            Ok(())
        })
    }

    fn send_cached_instruments(&self, instrument_id: Option<&InstrumentId>) {
        for instrument in self.feed.http.instruments().all() {
            if instrument_id.is_none_or(|id| *id == instrument.id()) {
                self.feed.send(instrument.into());
            }
        }
    }

    fn spawn_request(
        &self,
        what: &'static str,
        request: impl Future<Output = anyhow::Result<DataResponse>> + Send + 'static,
    ) {
        let sender = self.feed.sender.clone();
        get_runtime().spawn(async move {
            match request.await {
                Ok(response) => {
                    if let Err(e) = sender.send(DataEvent::Response(response)) {
                        log::error!("Failed to send TxFlow {what} response: {e}");
                    }
                }
                Err(e) => log::error!("TxFlow {what} request failed: {e:?}"),
            }
        });
    }

    fn abort_tasks(&mut self) {
        if let Some(task) = self.feed_task.take() {
            task.abort();
        }
        self.is_connected = false;
    }

    fn stop_in_background(&mut self) {
        self.abort_tasks();
        self.close_task = Some(get_runtime().spawn(self.feed.clone().close()));
    }
}

fn validate_book_type(book_type: BookType) -> anyhow::Result<()> {
    anyhow::ensure!(
        book_type == BookType::L2_MBP,
        "TxFlow serves L2_MBP books only, was {book_type}"
    );
    Ok(())
}

#[async_trait::async_trait(?Send)]
impl DataClient for TxflowDataClient {
    fn client_id(&self) -> ClientId {
        self.client_id
    }

    fn venue(&self) -> Option<Venue> {
        Some(*TXFLOW_VENUE)
    }

    fn start(&mut self) -> anyhow::Result<()> {
        log::info!(
            "Starting TxFlow data client {} ({})",
            self.client_id,
            self.config.environment
        );
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.stop_in_background();
        Ok(())
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        self.stop_in_background();
        Ok(())
    }

    fn dispose(&mut self) -> anyhow::Result<()> {
        self.stop_in_background();
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.is_connected
    }

    fn is_disconnected(&self) -> bool {
        !self.is_connected
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        if self.is_connected {
            return Ok(());
        }
        // A stop still closing the sockets must not close the new connection
        if let Some(task) = self.close_task.take() {
            let _ = task.await;
        }

        let instruments = self
            .feed
            .http
            .request_instruments()
            .await
            .context("failed to load TxFlow instruments")?;
        let count = instruments.len();
        for instrument in instruments {
            self.feed.send(instrument.into());
        }

        let node_rx = self
            .feed
            .node
            .lock()
            .await
            .connect()
            .await
            .context("failed to connect TxFlow node WebSocket")?;
        let api_rx = match self.feed.api.lock().await.connect().await {
            Ok(rx) => rx,
            Err(e) => {
                self.feed.node.lock().await.disconnect().await;
                return Err(e).context("failed to connect TxFlow API WebSocket");
            }
        };
        self.feed_task = Some(get_runtime().spawn(self.feed.clone().run(node_rx, api_rx)));
        self.is_connected = true;
        log::info!(
            "Connected TxFlow data client {}: {count} instruments",
            self.client_id
        );
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        self.abort_tasks();
        if let Some(task) = self.close_task.take() {
            let _ = task.await;
        }
        self.feed.clone().close().await;
        log::info!("Disconnected TxFlow data client {}", self.client_id);
        Ok(())
    }

    fn subscribe_instruments(&mut self, _cmd: SubscribeInstruments) -> anyhow::Result<()> {
        // Instruments are loaded at connect: replay them
        self.send_cached_instruments(None);
        Ok(())
    }

    fn subscribe_instrument(&mut self, cmd: SubscribeInstrument) -> anyhow::Result<()> {
        execute_instrument_subscriptions(&cmd.instrument_ids, |instrument_id| {
            self.instrument(&instrument_id)?;
            self.send_cached_instruments(Some(&instrument_id));
            Ok(())
        })
    }

    fn subscribe_book_deltas(&mut self, cmd: SubscribeBookDeltas) -> anyhow::Result<()> {
        validate_book_type(cmd.book_type)?;
        self.subscribe_kind(Kind::Deltas, &cmd.instrument_ids)
    }

    fn subscribe_quotes(&mut self, cmd: SubscribeQuotes) -> anyhow::Result<()> {
        self.subscribe_kind(Kind::Quotes, &cmd.instrument_ids)
    }

    fn subscribe_trades(&mut self, cmd: SubscribeTrades) -> anyhow::Result<()> {
        self.subscribe_kind(Kind::Trades, &cmd.instrument_ids)
    }

    fn subscribe_mark_prices(&mut self, cmd: SubscribeMarkPrices) -> anyhow::Result<()> {
        self.subscribe_kind(Kind::MarkPrices, &cmd.instrument_ids)
    }

    fn subscribe_index_prices(&mut self, cmd: SubscribeIndexPrices) -> anyhow::Result<()> {
        self.subscribe_kind(Kind::IndexPrices, &cmd.instrument_ids)
    }

    fn subscribe_funding_rates(&mut self, cmd: SubscribeFundingRates) -> anyhow::Result<()> {
        self.subscribe_kind(Kind::FundingRates, &cmd.instrument_ids)
    }

    fn subscribe_bars(&mut self, cmd: SubscribeBars) -> anyhow::Result<()> {
        execute_bar_subscriptions(&cmd.bar_types, |bar_type| {
            let key = self.candle_key(&bar_type)?;
            if self.feed.bars.insert(key.clone(), bar_type).is_none() {
                self.spawn_candles(key, true);
            }
            Ok(())
        })
    }

    fn unsubscribe_instruments(&mut self, _cmd: &UnsubscribeInstruments) -> anyhow::Result<()> {
        Ok(())
    }

    fn unsubscribe_instrument(&mut self, _cmd: &UnsubscribeInstrument) -> anyhow::Result<()> {
        Ok(())
    }

    fn unsubscribe_book_deltas(&mut self, cmd: &UnsubscribeBookDeltas) -> anyhow::Result<()> {
        self.unsubscribe_kind(Kind::Deltas, &cmd.instrument_ids)
    }

    fn unsubscribe_quotes(&mut self, cmd: &UnsubscribeQuotes) -> anyhow::Result<()> {
        self.unsubscribe_kind(Kind::Quotes, &cmd.instrument_ids)
    }

    fn unsubscribe_trades(&mut self, cmd: &UnsubscribeTrades) -> anyhow::Result<()> {
        self.unsubscribe_kind(Kind::Trades, &cmd.instrument_ids)
    }

    fn unsubscribe_mark_prices(&mut self, cmd: &UnsubscribeMarkPrices) -> anyhow::Result<()> {
        self.unsubscribe_kind(Kind::MarkPrices, &cmd.instrument_ids)
    }

    fn unsubscribe_index_prices(&mut self, cmd: &UnsubscribeIndexPrices) -> anyhow::Result<()> {
        self.unsubscribe_kind(Kind::IndexPrices, &cmd.instrument_ids)
    }

    fn unsubscribe_funding_rates(&mut self, cmd: &UnsubscribeFundingRates) -> anyhow::Result<()> {
        self.unsubscribe_kind(Kind::FundingRates, &cmd.instrument_ids)
    }

    fn unsubscribe_bars(&mut self, cmd: &UnsubscribeBars) -> anyhow::Result<()> {
        execute_bar_subscriptions(&cmd.bar_types, |bar_type| {
            let key = self.candle_key(&bar_type)?;
            if self.feed.bars.remove(&key).is_some() {
                self.spawn_candles(key, false);
            }
            Ok(())
        })
    }

    fn request_instruments(&self, request: RequestInstruments) -> anyhow::Result<()> {
        let feed = self.feed.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);

        self.spawn_request("instruments", async move {
            let instruments = feed.http.request_instruments().await?;
            Ok(DataResponse::Instruments(InstrumentsResponse::new(
                request.request_id,
                client_id,
                *TXFLOW_VENUE,
                instruments,
                datetime_to_unix_nanos(request.start),
                datetime_to_unix_nanos(request.end),
                feed.clock.get_time_ns(),
                request.params,
            )))
        });
        Ok(())
    }

    fn request_instrument(&self, request: RequestInstrument) -> anyhow::Result<()> {
        let instrument_id = request.single_instrument_id()?;
        let feed = self.feed.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);

        self.spawn_request("instrument", async move {
            // The venue has no query for one market
            feed.http.request_instruments().await?;
            let instrument = feed
                .http
                .instruments()
                .get(&instrument_id)
                .ok_or_else(|| InstrumentLookupError::not_found(instrument_id))?;
            Ok(DataResponse::Instrument(Box::new(InstrumentResponse::new(
                request.request_id,
                client_id,
                instrument_id,
                instrument,
                datetime_to_unix_nanos(request.start),
                datetime_to_unix_nanos(request.end),
                feed.clock.get_time_ns(),
                request.params,
            ))))
        });
        Ok(())
    }

    fn request_book_snapshot(&self, request: RequestBookSnapshot) -> anyhow::Result<()> {
        let instrument_id = request.single_instrument_id()?;
        self.instrument(&instrument_id)?;
        let feed = self.feed.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);

        self.spawn_request("book snapshot", async move {
            // The venue serves 20 levels per side; `request.depth` cannot raise that
            let deltas = feed.http.request_book_snapshot(instrument_id).await?;
            let mut book = OrderBook::new(instrument_id, BookType::L2_MBP);
            book.apply_deltas(&deltas)?;
            Ok(DataResponse::Book(BookResponse::new(
                request.request_id,
                client_id,
                instrument_id,
                book,
                None,
                None,
                feed.clock.get_time_ns(),
                request.params,
            )))
        });
        Ok(())
    }

    fn request_trades(&self, request: RequestTrades) -> anyhow::Result<()> {
        let instrument_id = request.single_instrument_id()?;
        self.instrument(&instrument_id)?;
        let feed = self.feed.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);
        let start = datetime_to_unix_nanos(request.start);
        let end = datetime_to_unix_nanos(request.end);
        let limit = request.limit.map(std::num::NonZeroUsize::get);

        self.spawn_request("trades", async move {
            // The venue serves its latest trades only (300 at most): no paging into history
            let mut trades = feed.http.request_trades(instrument_id).await?;
            trades.retain(|trade| {
                start.is_none_or(|start| trade.ts_event >= start)
                    && end.is_none_or(|end| trade.ts_event <= end)
            });
            if let Some(limit) = limit {
                trades.drain(..trades.len().saturating_sub(limit));
            }
            Ok(DataResponse::Trades(TradesResponse::new(
                request.request_id,
                client_id,
                instrument_id,
                trades,
                start,
                end,
                feed.clock.get_time_ns(),
                request.params,
            )))
        });
        Ok(())
    }

    fn request_funding_rates(&self, request: RequestFundingRates) -> anyhow::Result<()> {
        let instrument_id = request.single_instrument_id()?;
        self.instrument(&instrument_id)?;
        let feed = self.feed.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);
        let start = datetime_to_unix_nanos(request.start);
        let end = datetime_to_unix_nanos(request.end);

        self.spawn_request("funding rates", async move {
            // The venue has no public funding history: the answer is the current rate
            let funding_rates = vec![feed.http.request_funding_rate(instrument_id).await?];
            Ok(DataResponse::FundingRates(FundingRatesResponse::new(
                request.request_id,
                client_id,
                instrument_id,
                funding_rates,
                start,
                end,
                feed.clock.get_time_ns(),
                request.params,
            )))
        });
        Ok(())
    }

    fn request_bars(&self, request: RequestBars) -> anyhow::Result<()> {
        let bar_type = request.single_bar_type()?;
        bar_interval(&bar_type)?;
        self.instrument(&bar_type.instrument_id())?;
        let feed = self.feed.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);
        let start = datetime_to_unix_nanos(request.start);
        let end = datetime_to_unix_nanos(request.end);
        let limit = request.limit.map(std::num::NonZeroUsize::get);

        self.spawn_request("bars", async move {
            let bars = feed.http.request_bars(bar_type, start, end, limit).await?;
            Ok(DataResponse::Bars(BarsResponse::new(
                request.request_id,
                client_id,
                bar_type,
                bars,
                start,
                end,
                feed.clock.get_time_ns(),
                request.params,
            )))
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn mark_index_and_funding_share_one_channel() {
        let channels: Vec<_> = Kind::ALL.iter().map(|kind| kind.channel()).collect();
        assert_eq!(
            channels,
            [
                "l2BookDepth",
                "bbo",
                "trades",
                "activeAssetCtx",
                "activeAssetCtx",
                "activeAssetCtx",
            ]
        );
    }
}
