//! HTTP client for `POST /info` (reads) and `POST /exchange` (signed actions).

use std::{collections::HashMap, num::NonZeroU32, sync::Arc};

use nautilus_core::{AtomicTime, UnixNanos, time::get_atomic_clock_realtime};
use nautilus_model::{
    data::{Bar, BarType, FundingRateUpdate, OrderBookDeltas, TradeTick},
    identifiers::InstrumentId,
    instruments::InstrumentAny,
};
use nautilus_network::{
    http::{
        HttpClient, HttpRedirectPolicy, HttpResponse, Method, create_standard_nautilus_headers,
        parse_retry_after,
    },
    ratelimiter::quota::Quota,
    retry::{RetryManager, create_http_retry_manager},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    book::snapshot_deltas,
    common::{
        consts::{
            API_VERSION, API_VERSION_HEADER, EXCHANGE_REQUESTS_PER_SEC, INFO_REQUESTS_PER_SEC,
            MAX_CANDLES, MAX_PAGE_ROWS,
        },
        enums::{TxflowEnvironment, bar_interval},
        instruments::InstrumentCache,
        parse::millis_to_nanos,
        urls::txflow_http_base_url,
    },
    http::{
        error::{TxflowHttpError, TxflowHttpResult, should_retry},
        models::{
            Candle, ClearinghouseState, ExchangeResponse, InfoEnvelope, L2Book, MarketTicker,
            OrderDetails, OrderRow, Paged, Trade, TxflowOrder, Universe, UserFill,
        },
        parse::{parse_bar, parse_instrument, parse_ticker_funding, parse_trade_tick},
    },
};

const HEADER_RETRY_AFTER: &str = "retry-after";
const INFO_BUCKET: &str = "info";
const EXCHANGE_BUCKET: &str = "exchange";

// shortcut: one history request reads at most 50 pages (100,000 rows) or 20 candle windows
// (60,000 bars) and then stops with a warning; raise these when a longer history is needed.
const MAX_PAGES: usize = 50;
const MAX_CANDLE_WINDOWS: usize = 20;

/// Reads return Nautilus market data types or venue models; `/exchange` returns the venue
/// response as it is.
#[derive(Clone, Debug)]
pub struct TxflowHttpClient {
    base_url: String,
    client: HttpClient,
    retry_manager: RetryManager<TxflowHttpError>,
    instruments: Arc<InstrumentCache>,
    clock: &'static AtomicTime,
}

impl TxflowHttpClient {
    /// # Errors
    ///
    /// Returns an error if the underlying HTTP client cannot be created.
    pub fn new(
        environment: TxflowEnvironment,
        base_url: Option<String>,
        timeout_secs: u64,
        proxy_url: Option<String>,
    ) -> TxflowHttpResult<Self> {
        let base_url = base_url
            .unwrap_or_else(|| txflow_http_base_url(environment).to_string())
            .trim_end_matches('/')
            .to_string();
        let quota = |per_second: u32| {
            NonZeroU32::new(per_second)
                .and_then(Quota::per_second)
                .ok_or_else(|| TxflowHttpError::InvalidRequest("zero rate limit".to_string()))
        };

        Ok(Self {
            base_url,
            client: HttpClient::builder()
                .redirect_policy(HttpRedirectPolicy::Reject)
                .header_keys(vec![HEADER_RETRY_AFTER.to_string()])
                .headers(create_standard_nautilus_headers().into_iter().collect())
                .default_quota(quota(INFO_REQUESTS_PER_SEC)?)
                .keyed_quotas(vec![(
                    EXCHANGE_BUCKET.to_string(),
                    quota(EXCHANGE_REQUESTS_PER_SEC)?,
                )])
                .timeout_secs(timeout_secs)
                .maybe_proxy_url(proxy_url)
                .build()?,
            retry_manager: create_http_retry_manager(),
            instruments: Arc::new(InstrumentCache::default()),
            clock: get_atomic_clock_realtime(),
        })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The instruments loaded by [`Self::request_instruments`], shared with the clients.
    #[must_use]
    pub fn instruments(&self) -> &Arc<InstrumentCache> {
        &self.instruments
    }

    /// Overrides the retry manager. For mock-server tests that need a short backoff.
    pub fn set_retry_manager(&mut self, retry_manager: RetryManager<TxflowHttpError>) {
        self.retry_manager = retry_manager;
    }

    fn ts_init(&self) -> UnixNanos {
        self.clock.get_time_ns()
    }

    async fn post(
        &self,
        endpoint: &str,
        body: &Value,
        bucket: &str,
    ) -> TxflowHttpResult<HttpResponse> {
        let headers = HashMap::from([
            ("Content-Type".to_string(), "application/json".to_string()),
            (API_VERSION_HEADER.to_string(), API_VERSION.to_string()),
        ]);
        Ok(self
            .client
            .request(
                Method::POST,
                format!("{}{endpoint}", self.base_url),
                None,
                Some(headers),
                Some(serde_json::to_vec(body)?),
                None,
                Some(vec![bucket.to_string()]),
            )
            .await?)
    }

    /// Calls `POST /info`. Transport errors, HTTP 429 and HTTP 5xx are retried.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, the venue answers `success: false`, or the
    /// payload cannot be decoded.
    pub async fn info<T: DeserializeOwned>(&self, body: &Value) -> TxflowHttpResult<T> {
        let name = body.get("type").and_then(Value::as_str).unwrap_or("info");
        self.retry_manager
            .invocation(
                name,
                || async move {
                    let response = self.post("/info", body, INFO_BUCKET).await?;
                    Self::parse_info(&response)
                },
                should_retry,
                |e| TxflowHttpError::Network(e.to_string()),
            )
            .retry_delay(&TxflowHttpError::retry_after)
            .execute()
            .await
    }

    /// Calls `POST /exchange` with a signed payload. Never retried: a retry needs a new nonce
    /// and a new signature, and after a transport error the first attempt may have executed.
    ///
    /// # Errors
    ///
    /// Returns [`TxflowHttpError::RateLimit`] for HTTP 429 (the action did not execute), and
    /// another error if the request fails or the body is not an exchange response. A venue
    /// rejection is `Ok`: see [`ExchangeResponse::reject_reason`].
    pub async fn exchange(&self, payload: &Value) -> TxflowHttpResult<ExchangeResponse> {
        let response = self.post("/exchange", payload, EXCHANGE_BUCKET).await?;
        let status = response.status.as_u16();
        let body = String::from_utf8_lossy(&response.body).to_string();
        let retry_after = Self::retry_after(&response);
        if status == 429 {
            return Err(TxflowHttpError::RateLimit {
                message: body,
                retry_after,
            });
        }
        serde_json::from_slice(&response.body).map_err(|_| TxflowHttpError::Http {
            status,
            body,
            retry_after,
        })
    }

    fn retry_after(response: &HttpResponse) -> Option<std::time::Duration> {
        let value = response.headers.get(HEADER_RETRY_AFTER)?;
        let now = get_atomic_clock_realtime().get_time_ns().to_datetime_utc();
        parse_retry_after(value, now)
    }

    fn parse_info<T: DeserializeOwned>(response: &HttpResponse) -> TxflowHttpResult<T> {
        if !response.status.is_success() {
            let retry_after = Self::retry_after(response);
            let status = response.status.as_u16();
            let body = String::from_utf8_lossy(&response.body).to_string();

            // Status first: an error body must not override the retry decision.
            if status == 429 {
                return Err(TxflowHttpError::RateLimit {
                    message: body,
                    retry_after,
                });
            }
            if status < 500
                && let Ok(envelope) = serde_json::from_slice::<InfoEnvelope<Value>>(&response.body)
                && !envelope.success
            {
                return Err(TxflowHttpError::Venue {
                    code: envelope.code,
                    message: envelope.message.unwrap_or_default(),
                });
            }
            return Err(TxflowHttpError::Http {
                status,
                body,
                retry_after,
            });
        }

        // The venue answers an unknown market with HTTP 200 and `success: false`
        let envelope: InfoEnvelope<T> = serde_json::from_slice(&response.body)?;
        if !envelope.success {
            return Err(TxflowHttpError::Venue {
                code: envelope.code,
                message: envelope.message.unwrap_or_default(),
            });
        }
        envelope
            .data
            .ok_or_else(|| TxflowHttpError::Parse("response without data".to_string()))
    }

    /// The `coin` of a request (the asset index as text) and the cached instrument.
    fn coin(&self, instrument_id: &InstrumentId) -> TxflowHttpResult<(String, InstrumentAny)> {
        self.instruments
            .asset(instrument_id)
            .zip(self.instruments.get(instrument_id))
            .map(|(asset, instrument)| (asset.to_string(), instrument))
            .ok_or_else(|| {
                TxflowHttpError::InvalidRequest(format!("unknown instrument {instrument_id}"))
            })
    }

    /// Loads every perpetual market that is not delisted and fills the instrument cache.
    /// A halted market is loaded: it can trade again without a new listing.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails.
    pub async fn request_instruments(&self) -> TxflowHttpResult<Vec<InstrumentAny>> {
        let meta: Universe = self.info(&json!({"type": "perpMeta"})).await?;
        let ts_init = self.ts_init();
        let mut instruments = Vec::with_capacity(meta.universe.len());
        for market in meta.universe.iter().filter(|market| !market.delisted) {
            match parse_instrument(market, ts_init) {
                Ok(instrument) => {
                    self.instruments.insert(
                        market.index,
                        &market.name,
                        &market.base_currency,
                        instrument.clone(),
                    );
                    instruments.push(instrument);
                }
                Err(e) => log::warn!("Skipping market {}: {e}", market.name),
            }
        }
        Ok(instruments)
    }

    /// Book snapshot of 20 levels per side, as deltas that replace the book.
    ///
    /// # Errors
    ///
    /// Returns an error if the instrument is unknown, the request fails, or a level is invalid.
    pub async fn request_book_snapshot(
        &self,
        instrument_id: InstrumentId,
    ) -> TxflowHttpResult<OrderBookDeltas> {
        let (coin, instrument) = self.coin(&instrument_id)?;
        let book: L2Book = self.info(&json!({"type": "l2Book", "coin": coin})).await?;
        let ts_event = millis_to_nanos(book.time);
        Ok(snapshot_deltas(
            &instrument,
            &book.levels,
            0,
            ts_event,
            self.ts_init(),
        )?)
    }

    /// The most recent public trades (the venue returns up to 300), oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error if the instrument is unknown or the request fails.
    pub async fn request_trades(
        &self,
        instrument_id: InstrumentId,
    ) -> TxflowHttpResult<Vec<TradeTick>> {
        let (coin, instrument) = self.coin(&instrument_id)?;
        let rows: Vec<Trade> = self.info(&json!({"type": "trades", "coin": coin})).await?;
        let ts_init = self.ts_init();
        let mut trades: Vec<TradeTick> = rows
            .iter()
            .filter_map(|row| {
                parse_trade_tick(row, &instrument, ts_init)
                    .map_err(|e| log::warn!("Skipping trade {}: {e}", row.tid))
                    .ok()
            })
            .collect();
        trades.sort_by_key(|trade| trade.ts_event);
        Ok(trades)
    }

    /// Closed bars of `[start, end)`, oldest first. Without `start` the range is the `limit`
    /// bars (at most 3000) before `end`. With `limit` the newest `limit` bars are kept.
    ///
    /// # Errors
    ///
    /// Returns an error if the bar type is not supported, the instrument is unknown, or a
    /// request fails.
    pub async fn request_bars(
        &self,
        bar_type: BarType,
        start: Option<UnixNanos>,
        end: Option<UnixNanos>,
        limit: Option<usize>,
    ) -> TxflowHttpResult<Vec<Bar>> {
        let (interval, step_ms) =
            bar_interval(&bar_type).map_err(|e| TxflowHttpError::InvalidRequest(e.to_string()))?;
        let (coin, instrument) = self.coin(&bar_type.instrument_id())?;
        let to_ms = |ts: UnixNanos| ts.as_u64() / 1_000_000;
        let now_ms = to_ms(self.ts_init());
        let end_ms = end.map_or(now_ms, to_ms).min(now_ms);
        let wanted = limit.filter(|limit| *limit > 0).unwrap_or(MAX_CANDLES);
        let default_span = step_ms.saturating_mul(wanted.min(MAX_CANDLES) as u64);
        let mut from = start.map_or_else(|| end_ms.saturating_sub(default_span), to_ms);
        // The venue answers a window of more than 3000 bars with an empty list
        let window = step_ms.saturating_mul(MAX_CANDLES as u64);

        let ts_init = self.ts_init();
        let mut bars = Vec::new();
        for page in 0.. {
            if from >= end_ms {
                break;
            }
            if page == MAX_CANDLE_WINDOWS {
                log::warn!("Bars request for {bar_type} stopped at {MAX_CANDLE_WINDOWS} windows");
                break;
            }
            let to = from.saturating_add(window).min(end_ms);
            let request =
                json!({"coin": coin, "interval": interval, "startTime": from, "endTime": to});
            let candles: Vec<Candle> = self
                .info(&json!({"type": "candleSnapshot", "req": request}))
                .await?;
            // The newest candle is still open: its close time is not in the past
            for candle in candles.iter().filter(|candle| candle.close_time < now_ms) {
                match parse_bar(candle, bar_type, &instrument, ts_init) {
                    Ok(bar) => bars.push(bar),
                    Err(e) => log::warn!("Skipping candle {} of {bar_type}: {e}", candle.t),
                }
            }
            from = to;
        }
        bars.sort_by_key(|bar| bar.ts_event);
        if let Some(limit) = limit.filter(|limit| *limit > 0 && *limit < bars.len()) {
            bars.drain(..bars.len() - limit);
        }
        Ok(bars)
    }

    /// The funding rate of the current hour. The venue has no public funding history.
    ///
    /// # Errors
    ///
    /// Returns an error if the instrument is unknown or the request fails.
    pub async fn request_funding_rate(
        &self,
        instrument_id: InstrumentId,
    ) -> TxflowHttpResult<FundingRateUpdate> {
        let asset = self.instruments.asset(&instrument_id).ok_or_else(|| {
            TxflowHttpError::InvalidRequest(format!("unknown instrument {instrument_id}"))
        })?;
        let ticker: MarketTicker = self
            .info(&json!({"type": "marketTicker", "instrumentId": asset}))
            .await?;
        Ok(parse_ticker_funding(&ticker, instrument_id, self.ts_init()))
    }

    /// Open orders of `user`, every page.
    ///
    /// # Errors
    ///
    /// Returns an error if a request fails.
    pub async fn request_open_orders(&self, user: &str) -> TxflowHttpResult<Vec<TxflowOrder>> {
        let mut orders = Vec::new();
        for _ in 0..MAX_PAGES {
            let page: Vec<TxflowOrder> = self
                .info(&json!({
                    "type": "frontendOpenOrders",
                    "user": user,
                    "limit": MAX_PAGE_ROWS,
                    "offset": orders.len(),
                }))
                .await?;
            let full = page.len() >= MAX_PAGE_ROWS;
            orders.extend(page);
            if !full {
                return Ok(orders);
            }
        }
        log::warn!("Open orders request stopped at {MAX_PAGES} pages");
        Ok(orders)
    }

    /// Pending trigger (stop) orders of `user`. The venue keeps them outside the book and
    /// outside `frontendOpenOrders`. A row that cannot be decoded is logged and skipped.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails.
    pub async fn request_trigger_orders(&self, user: &str) -> TxflowHttpResult<Vec<TxflowOrder>> {
        let rows: Vec<Value> = self
            .info(&json!({"type": "triggerOrders", "user": user}))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| match serde_json::from_value::<TxflowOrder>(row) {
                Ok(mut order) => {
                    order.is_trigger = true;
                    Some(order)
                }
                Err(e) => {
                    log::warn!("Skipping a trigger order that cannot be decoded: {e}");
                    None
                }
            })
            .collect())
    }

    /// Orders of `user` in any state, newest first, every page.
    ///
    /// # Errors
    ///
    /// Returns an error if a request fails.
    pub async fn request_historical_orders(
        &self,
        user: &str,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
    ) -> TxflowHttpResult<Vec<OrderRow>> {
        self.paged("historicalOrders", user, start_ms, end_ms).await
    }

    /// Fills of `user`, newest first, every page.
    ///
    /// # Errors
    ///
    /// Returns an error if a request fails.
    pub async fn request_user_fills(
        &self,
        user: &str,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
    ) -> TxflowHttpResult<Vec<UserFill>> {
        self.paged("userFills", user, start_ms, end_ms).await
    }

    async fn paged<T: DeserializeOwned>(
        &self,
        kind: &str,
        user: &str,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
    ) -> TxflowHttpResult<Vec<T>> {
        let mut rows = Vec::new();
        for _ in 0..MAX_PAGES {
            let mut body = json!({
                "type": kind,
                "user": user,
                "limit": MAX_PAGE_ROWS,
                "offset": rows.len(),
            });
            if let Some(start) = start_ms {
                body["startTimeMs"] = start.into();
            }
            if let Some(end) = end_ms {
                body["endTimeMs"] = end.into();
            }
            let page: Paged<T> = self.info(&body).await?;
            let empty = page.data.is_empty();
            rows.extend(page.data);
            if !page.has_more || empty {
                return Ok(rows);
            }
        }
        log::warn!("{kind} request stopped at {MAX_PAGES} pages");
        Ok(rows)
    }

    /// One order by its venue order id.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or the venue does not know the order.
    pub async fn request_order_details(&self, oid: u64) -> TxflowHttpResult<OrderRow> {
        let details: OrderDetails = self
            .info(&json!({"type": "orderDetails", "orderId": oid}))
            .await?;
        Ok(details.order_details)
    }

    /// Margin summary and positions of `user`.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails.
    pub async fn request_clearinghouse_state(
        &self,
        user: &str,
    ) -> TxflowHttpResult<ClearinghouseState> {
        self.info(&json!({"type": "clearinghouseState", "user": user}))
            .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use nautilus_model::{enums::BookAction, instruments::Instrument};
    use nautilus_network::retry::RetryConfig;
    use rstest::rstest;

    use super::*;

    pub(crate) fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap()
    }

    pub(crate) async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    /// Client for the mock server with a backoff of one millisecond.
    pub(crate) fn client(base_url: String, max_retries: u32) -> TxflowHttpClient {
        let mut client =
            TxflowHttpClient::new(TxflowEnvironment::Testnet, Some(base_url), 5, None).unwrap();
        client.set_retry_manager(RetryManager::new(RetryConfig {
            max_retries,
            initial_delay_ms: 1,
            max_delay_ms: 1,
            backoff_factor: 1.0,
            jitter_ms: 0,
            operation_timeout_ms: Some(5_000),
            immediate_first: false,
            max_elapsed_ms: None,
        }));
        client
    }

    type Requests = Arc<Mutex<Vec<Value>>>;

    /// `/info` mock: answers by request `type` from the fixtures and records every body.
    pub(crate) fn info_router(requests: Requests) -> Router {
        Router::new().route(
            "/info",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                assert_eq!(headers[API_VERSION_HEADER], API_VERSION);
                let kind = body["type"].as_str().unwrap().to_string();
                requests.lock().unwrap().push(body);
                async move { fixture(&format!("{kind}.json")) }
            }),
        )
    }

    /// Route that counts its calls and always answers with `status` and `body`.
    fn counted(
        status: StatusCode,
        body: &'static str,
    ) -> (Arc<AtomicUsize>, axum::routing::MethodRouter) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let handler = move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async move { (status, [(HEADER_RETRY_AFTER, "2")], body) }
        };
        (hits, post(handler))
    }

    fn btc() -> InstrumentId {
        InstrumentId::from("BTC-USDC-PERP.TXFLOW")
    }

    #[tokio::test]
    async fn market_data_requests_parse_the_live_fixtures() {
        let requests = Requests::default();
        let client = client(serve(info_router(requests.clone())).await, 0);

        let instruments = client.request_instruments().await.unwrap();
        // The fixture has four live markets and one delisted market
        assert_eq!(instruments.len(), 4);
        assert_eq!(client.instruments().asset(&btc()), Some(1));
        let doge = client.instruments().lookup("DOGE-USDC").unwrap();
        assert_eq!(doge.size_increment().to_string(), "10");
        assert_eq!(doge.price_increment().to_string(), "0.00001");

        let book = client.request_book_snapshot(btc()).await.unwrap();
        assert_eq!(book.deltas.len(), 41);
        assert_eq!(book.deltas[0].action, BookAction::Clear);

        let trades = client.request_trades(btc()).await.unwrap();
        assert_eq!(trades.len(), 6);
        assert!(trades.is_sorted_by_key(|trade| trade.ts_event));
        assert_eq!(trades[5].trade_id.to_string(), "370439476000002");

        let funding = client.request_funding_rate(btc()).await.unwrap();
        // 0.00079443 % per hour
        assert_eq!(funding.rate.to_string(), "0.0000079443");
        assert_eq!(funding.interval, Some(60));

        let bar_type = BarType::from("BTC-USDC-PERP.TXFLOW-1-MINUTE-LAST-EXTERNAL");
        let bars = client
            .request_bars(bar_type, None, None, Some(3))
            .await
            .unwrap();
        assert_eq!(bars.len(), 3);
        assert!(bars.is_sorted_by_key(|bar| bar.ts_event));

        let sent = requests.lock().unwrap();
        assert_eq!(sent[1], json!({"type": "l2Book", "coin": "1"}));
        assert_eq!(sent[3], json!({"type": "marketTicker", "instrumentId": 1}));
        let candles = &sent[4]["req"];
        assert_eq!(
            (&candles["coin"], &candles["interval"]),
            (&json!("1"), &json!("1m"))
        );
        let span = candles["endTime"].as_u64().unwrap() - candles["startTime"].as_u64().unwrap();
        assert_eq!(span, 3 * 60_000);
    }

    #[tokio::test]
    async fn bars_are_requested_in_windows_of_3000() {
        let requests = Requests::default();
        let client = client(serve(info_router(requests.clone())).await, 0);
        client.request_instruments().await.unwrap();

        let bar_type = BarType::from("BTC-USDC-PERP.TXFLOW-1-MINUTE-LAST-EXTERNAL");
        let start = 1_790_000_000_000_u64; // far enough back that `end` is not clamped to now
        let end = start + 7000 * 60_000;
        let range = |ms: u64| Some(millis_to_nanos(ms));
        client
            .request_bars(bar_type, range(start), range(end), None)
            .await
            .unwrap();

        let sent = requests.lock().unwrap();
        let windows: Vec<(u64, u64)> = sent[1..]
            .iter()
            .map(|body| {
                let req = &body["req"];
                (
                    req["startTime"].as_u64().unwrap(),
                    req["endTime"].as_u64().unwrap(),
                )
            })
            .collect();
        let step = 3000 * 60_000;
        assert_eq!(
            windows,
            [
                (start, start + step),
                (start + step, start + 2 * step),
                (start + 2 * step, end)
            ]
        );
    }

    #[tokio::test]
    async fn venue_error_with_http_200_is_not_retried() {
        let (hits, route) = counted(
            StatusCode::OK,
            r#"{"success":false,"code":21001,"message":"Orderbook for coin '99999' not found"}"#,
        );
        let client = client(serve(Router::new().route("/info", route)).await, 3);

        let error = client
            .info::<Value>(&json!({"type": "l2Book"}))
            .await
            .unwrap_err();

        assert!(
            matches!(error, TxflowHttpError::Venue { code: 21001, .. }),
            "{error}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[rstest]
    #[case(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"success":false,"code":10000,"message":"rate limit exceeded"}"#
    )]
    #[case(StatusCode::BAD_GATEWAY, "bad gateway")]
    #[tokio::test]
    async fn rate_limit_and_5xx_are_retried_with_retry_after(
        #[case] status: StatusCode,
        #[case] body: &'static str,
    ) {
        let (hits, route) = counted(status, body);
        let client = client(serve(Router::new().route("/info", route)).await, 2);

        let error = client
            .info::<Value>(&json!({"type": "perpMeta"}))
            .await
            .unwrap_err();

        assert_eq!(hits.load(Ordering::SeqCst), 3);
        assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(2)));
        assert_eq!(
            matches!(error, TxflowHttpError::RateLimit { .. }),
            status == StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn user_history_is_read_page_by_page() {
        let requests = Requests::default();
        let seen = requests.clone();
        let fill = |oid: u64| json!({"coin": "BTC", "instrumentId": 1, "oid": oid, "side": "B", "px": "50000.0", "sz": "0.01", "time": 1, "fee": "0.2", "feeToken": "USDC", "tradeSide": "Taker"});
        let router = Router::new().route(
            "/info",
            post(move |Json(body): Json<Value>| {
                let first = body["offset"] == 0;
                seen.lock().unwrap().push(body);
                let rows = if first { vec![fill(1), fill(2)] } else { vec![fill(3)] };
                async move { Json(json!({"success": true, "code": 0, "data": {"data": rows, "hasMore": first}})) }
            }),
        );
        let client = client(serve(router).await, 0);

        let fills = client
            .request_user_fills("0xabc", Some(5), None)
            .await
            .unwrap();

        assert_eq!(
            fills.iter().map(|fill| fill.oid).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let sent = requests.lock().unwrap();
        assert_eq!(
            sent[1],
            json!({"type": "userFills", "user": "0xabc", "limit": 2000, "offset": 2, "startTimeMs": 5})
        );
    }

    #[rstest]
    #[case(
        StatusCode::OK,
        r#"{"status":"ok","code":0,"data":{"type":"PlaceOrder","data":{"statuses":["success"]}}}"#,
        Ok(None)
    )]
    #[case(
        StatusCode::BAD_REQUEST,
        r#"{"status":"err","code":10000,"message":"Order not found"}"#,
        Ok(Some("Order not found"))
    )]
    #[case(
        StatusCode::ACCEPTED,
        r#"{"status":"pending","txHash":"0xab"}"#,
        Ok(None)
    )]
    #[case(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"status":"err","code":10000,"response":"rate limit exceeded"}"#,
        Err("rate limit")
    )]
    #[case(StatusCode::BAD_GATEWAY, "bad gateway", Err("HTTP 502"))]
    #[tokio::test]
    async fn exchange_is_sent_once_and_maps_the_answer(
        #[case] status: StatusCode,
        #[case] body: &'static str,
        #[case] expected: Result<Option<&str>, &str>,
    ) {
        let (hits, route) = counted(status, body);
        let client = client(serve(Router::new().route("/exchange", route)).await, 3);

        let result = client.exchange(&json!({"action": {}})).await;

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        match (result, expected) {
            (Ok(response), Ok(reason)) => assert_eq!(response.reject_reason().as_deref(), reason),
            (Err(error), Err(text)) => assert!(error.to_string().contains(text), "{error}"),
            (result, expected) => panic!("{result:?} does not match {expected:?}"),
        }
    }
}
