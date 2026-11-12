//! Live execution client for TxFlow perpetuals.
//!
//! Orders go out as signed `/exchange` actions. The answer says only whether the action ran,
//! so acceptance, fills and cancels come from the `orderUpdates` channel of the node socket
//! (see [`dispatch`]). Reports for reconciliation come from `/info`.

pub mod dispatch;
pub mod order;

use std::{collections::HashSet, str::FromStr, sync::Arc, time::Duration};

use alloy::primitives::Address;
use anyhow::Context;
use async_trait::async_trait;
use nautilus_common::{
    clients::ExecutionClient,
    live::{get_runtime, runner::get_exec_event_sender},
    messages::execution::{
        BatchCancelOrders, CancelAllOrders, CancelOrder, GenerateFillReports,
        GenerateOrderStatusReport, GenerateOrderStatusReports, GeneratePositionStatusReports,
        ModifyOrder, QueryAccount, QueryOrder, SubmitOrder, SubmitOrderList,
    },
};
use nautilus_core::{
    UnixNanos, env::get_or_env_var_opt, params::Params, time::get_atomic_clock_realtime,
};
use nautilus_live::{ExecutionClientCore, ExecutionEventEmitter};
use nautilus_model::{
    accounts::AccountAny,
    enums::{OmsType, OrderType},
    identifiers::{
        AccountId, ClientId, ClientOrderId, InstrumentId, StrategyId, Venue, VenueOrderId,
    },
    instruments::Instrument,
    orders::{Order, OrderAny},
    reports::{FillReport, OrderStatusReport, PositionStatusReport},
    types::{AccountBalance, MarginBalance, Price, Quantity},
};
use serde_json::json;
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use self::{
    dispatch::OrderDispatch,
    order::{Verdict, order_body, verdicts},
};
use crate::{
    common::{
        consts::{
            ENV_ACCOUNT_ADDRESS, ENV_AGENT_PRIVATE_KEY, ENV_TESTNET_ACCOUNT_ADDRESS,
            ENV_TESTNET_AGENT_PRIVATE_KEY, MAX_CANCELS_PER_ACTION, MAX_ORDERS_PER_ACTION,
        },
        enums::{TxflowEnvironment, parse_order_status},
        instruments::InstrumentCache,
        parse::{millis_to_nanos, wire_decimal},
    },
    config::{exec::TxflowExecClientConfig, ws_urls},
    http::{
        client::TxflowHttpClient,
        error::{TxflowHttpError, TxflowHttpResult},
        models::{ClearinghouseState, ExchangeResponse, OrderRow, Position, TxflowOrder},
        parse::{
            parse_account_balance, parse_fill_reports, parse_order_status_report,
            parse_position_status_report,
        },
    },
    signing::{
        TxflowSigner,
        action::{
            Action, CancelAction, CancelBody, CancelByCloidAction, CancelByCloidBody,
            CancelGrouping, Grouping, ModifyAction, ModifyBody, OrderAction, OrderBody,
            UpdateLeverage,
        },
        nonce::NonceManager,
    },
    websocket::{client::TxflowWebSocketClient, messages::WsMessage},
};

const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How far before the connect time the order history is read again after a reconnect.
const CATCH_UP_OVERLAP_MS: u64 = 60_000;

fn now() -> UnixNanos {
    get_atomic_clock_realtime().get_time_ns()
}

fn spawn(future: impl Future<Output = ()> + Send + 'static) -> JoinHandle<()> {
    get_runtime().spawn(future)
}

fn is_trigger(order_type: OrderType) -> bool {
    !matches!(order_type, OrderType::Market | OrderType::Limit)
}

/// A row of the open or trigger order lists, which carry no row status.
fn open_row(order: TxflowOrder) -> OrderRow {
    // unverified: the values of `triggerState`. A listed order with no readable state is open
    let status = [order.status.as_deref(), order.state.as_deref()]
        .into_iter()
        .flatten()
        .find(|status| parse_order_status(status).is_ok())
        .unwrap_or("open")
        .to_string();
    OrderRow {
        order,
        status,
        status_timestamp: None,
    }
}

#[derive(Debug, Clone)]
struct Cancel {
    strategy_id: StrategyId,
    instrument_id: InstrumentId,
    client_order_id: ClientOrderId,
    venue_order_id: Option<VenueOrderId>,
    /// Trigger orders are stored apart from the book and need their own cancel grouping.
    trigger: bool,
}

#[derive(Debug)]
struct Modify {
    order: OrderAny,
    venue_order_id: VenueOrderId,
    quantity: Quantity,
    price: Price,
    body: ModifyBody,
}

/// What the tasks of one connection share.
#[derive(Debug)]
struct Shared {
    http: TxflowHttpClient,
    signer: TxflowSigner,
    nonces: NonceManager,
    dispatch: OrderDispatch,
    emitter: ExecutionEventEmitter,
    instruments: Arc<InstrumentCache>,
    account_id: AccountId,
    /// Account address: the owner of orders and positions, not the agent that signs.
    user: String,
    connected_ms: u64,
}

impl Shared {
    async fn send(&self, action: &Action) -> TxflowHttpResult<ExchangeResponse> {
        let nonce = self.nonces.next(now().as_u64() / 1_000_000);
        let envelope = self
            .signer
            .envelope(action, nonce)
            .map_err(|e| TxflowHttpError::InvalidRequest(e.to_string()))?;
        self.http.exchange(&envelope).await
    }

    async fn submit(&self, grouping: Grouping, requests: &[(ClientOrderId, OrderBody)]) {
        for chunk in requests.chunks(MAX_ORDERS_PER_ACTION) {
            let orders = chunk.iter().map(|(_, body)| body.clone()).collect();
            let result = self
                .send(&Action::Order(OrderAction { grouping, orders }))
                .await;
            let mut sent = Vec::new();
            for ((client_order_id, _), verdict) in chunk.iter().zip(verdicts(&result, chunk.len()))
            {
                match verdict {
                    Verdict::Sent => sent.push(*client_order_id),
                    Verdict::Rejected(reason) => {
                        // unverified: the wording of the post-only reject
                        let due_post_only = reason.to_ascii_lowercase().contains("post");
                        self.dispatch
                            .rejected(client_order_id, &reason, now(), due_post_only);
                    }
                    Verdict::Unknown(reason) => log::error!(
                        "TxFlow gave no clear answer for order {client_order_id}, the order \
                         stream or the next reconciliation decides: {reason}"
                    ),
                }
            }
            if grouping == Grouping::NormalTrigger && !sent.is_empty() {
                self.accept_triggers(&sent).await;
            }
        }
    }

    /// Trigger orders are stored apart from the book and the order stream may not show them
    /// (unverified), so their venue order ids are read once after the action ran.
    async fn accept_triggers(&self, client_order_ids: &[ClientOrderId]) {
        let orders = match self.http.request_trigger_orders(&self.user).await {
            Ok(orders) => orders,
            Err(e) => {
                log::warn!("TxFlow trigger order lookup failed: {e}");
                return;
            }
        };
        for order in orders {
            let client_order_id = order
                .cloid
                .as_deref()
                .and_then(|cloid| ClientOrderId::new_checked(cloid).ok())
                .filter(|id| client_order_ids.contains(id));
            if let Some(client_order_id) = client_order_id {
                let ts_event = order.timestamp.map_or_else(now, millis_to_nanos);
                self.dispatch
                    .accepted(&client_order_id, order.oid, ts_event);
            }
        }
    }

    fn cancel_rejected(&self, cancel: &Cancel, reason: &str) {
        self.emitter.emit_order_cancel_rejected_event(
            cancel.strategy_id,
            cancel.instrument_id,
            cancel.client_order_id,
            cancel.venue_order_id,
            reason,
            now(),
        );
    }

    /// Cancels by venue order id where one is known, by client order id otherwise.
    async fn cancel(&self, cancels: Vec<Cancel>) {
        let mut by_oid = Vec::new();
        let mut by_cloid = Vec::new();
        for cancel in cancels {
            let Some(asset) = self.instruments.asset(&cancel.instrument_id) else {
                self.cancel_rejected(&cancel, "unknown instrument");
                continue;
            };
            let grouping = cancel.trigger.then_some(CancelGrouping::Trigger);
            let oid = self
                .dispatch
                .venue_order_id(&cancel.client_order_id)
                .or(cancel.venue_order_id)
                .and_then(|id| id.as_str().parse::<u64>().ok());
            match oid {
                Some(o) => by_oid.push((
                    CancelBody {
                        a: asset,
                        o,
                        grouping,
                    },
                    cancel,
                )),
                None => by_cloid.push((
                    CancelByCloidBody {
                        asset,
                        cloid: cancel.client_order_id.to_string(),
                        grouping,
                    },
                    cancel,
                )),
            }
        }
        for chunk in by_oid.chunks(MAX_CANCELS_PER_ACTION) {
            let (cancels, orders): (Vec<_>, Vec<_>) = chunk.iter().cloned().unzip();
            self.send_cancels(&Action::Cancel(CancelAction { cancels }), &orders)
                .await;
        }
        for chunk in by_cloid.chunks(MAX_CANCELS_PER_ACTION) {
            let (cancels, orders): (Vec<_>, Vec<_>) = chunk.iter().cloned().unzip();
            let action = Action::CancelByCloid(CancelByCloidAction { cancels });
            self.send_cancels(&action, &orders).await;
        }
    }

    async fn send_cancels(&self, action: &Action, cancels: &[Cancel]) {
        let result = self.send(action).await;
        for (cancel, verdict) in cancels.iter().zip(verdicts(&result, cancels.len())) {
            match verdict {
                // The `Canceled` row of the order stream closes a book order. A trigger order
                // may never get a row (unverified), so the answer closes it.
                Verdict::Sent if cancel.trigger => {
                    self.dispatch.canceled(&cancel.client_order_id, now());
                }
                Verdict::Sent => {}
                Verdict::Rejected(reason) | Verdict::Unknown(reason) => {
                    self.cancel_rejected(cancel, &reason);
                }
            }
        }
    }

    async fn modify(&self, modify: Modify) {
        let action = Action::Modify2(ModifyAction { order: modify.body });
        let result = self.send(&action).await;
        match verdicts(&result, 1).pop() {
            // `modify2` amends in place: the order keeps its ids (docs; not seen live)
            Some(Verdict::Sent) => self.emitter.emit_order_updated(
                &modify.order,
                modify.venue_order_id,
                modify.quantity,
                Some(modify.price),
                None,
                None,
                now(),
            ),
            Some(Verdict::Rejected(reason) | Verdict::Unknown(reason)) => {
                self.emitter.emit_order_modify_rejected_event(
                    modify.order.strategy_id(),
                    modify.order.instrument_id(),
                    modify.order.client_order_id(),
                    Some(modify.venue_order_id),
                    &reason,
                    now(),
                );
            }
            None => {}
        }
    }

    async fn update_leverage(
        &self,
        instrument_id: &InstrumentId,
        leverage: u32,
    ) -> anyhow::Result<()> {
        let asset = self
            .instruments
            .asset(instrument_id)
            .with_context(|| format!("unknown instrument {instrument_id} in the config"))?;
        let action = Action::UpdateLeverage(UpdateLeverage {
            asset,
            leverage: i32::try_from(leverage).context("leverage out of range")?,
            margin_mode: None,
        });
        match verdicts(&self.send(&action).await, 1).pop() {
            Some(Verdict::Rejected(reason) | Verdict::Unknown(reason)) => anyhow::bail!(reason),
            _ => Ok(()),
        }
    }

    fn emit_balance(&self, state: &ClearinghouseState) {
        match parse_account_balance(state) {
            Ok(balance) => {
                self.emitter
                    .emit_account_state(vec![balance], vec![], true, now(), None);
            }
            Err(e) => log::warn!("TxFlow account state unreadable: {e}"),
        }
    }

    async fn refresh_account(&self) -> anyhow::Result<()> {
        let state = self.http.request_clearinghouse_state(&self.user).await?;
        self.emit_balance(&state);
        Ok(())
    }

    fn since_ms(&self) -> u64 {
        self.connected_ms.saturating_sub(CATCH_UP_OVERLAP_MS)
    }

    /// Resting orders of the book and pending trigger orders.
    async fn open_rows(&self) -> anyhow::Result<Vec<OrderRow>> {
        let mut rows: Vec<OrderRow> = self
            .http
            .request_open_orders(&self.user)
            .await?
            .into_iter()
            .map(open_row)
            .collect();
        let listed: HashSet<u64> = rows.iter().map(|row| row.order.oid).collect();
        let triggers = self.http.request_trigger_orders(&self.user).await?;
        rows.extend(
            triggers
                .into_iter()
                .filter(|order| !listed.contains(&order.oid))
                .map(open_row),
        );
        Ok(rows)
    }

    fn report(&self, row: &OrderRow) -> Option<OrderStatusReport> {
        let order = &row.order;
        let Some(instrument) = self.instruments.lookup(&order.instrument_key()) else {
            log::warn!("Order {} is on unknown market {}", order.oid, order.coin);
            return None;
        };
        parse_order_status_report(
            order,
            &row.status,
            row.status_timestamp,
            self.account_id,
            &instrument,
            now(),
        )
        .map_err(|e| log::warn!("Skipping order {}: {e}", order.oid))
        .ok()
    }

    async fn order_reports(
        &self,
        open_only: bool,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
    ) -> anyhow::Result<Vec<OrderStatusReport>> {
        let mut rows = self.open_rows().await?;
        if !open_only {
            let open: HashSet<u64> = rows.iter().map(|row| row.order.oid).collect();
            let history = self
                .http
                .request_historical_orders(&self.user, start_ms, end_ms)
                .await?;
            rows.extend(
                history
                    .into_iter()
                    .filter(|row| !open.contains(&row.order.oid)),
            );
        }
        Ok(rows.iter().filter_map(|row| self.report(row)).collect())
    }

    /// The current row of one order: by venue order id, else from the open lists, else from
    /// the history since connect.
    async fn order_row(
        &self,
        client_order_id: Option<ClientOrderId>,
        venue_order_id: Option<VenueOrderId>,
    ) -> anyhow::Result<Option<OrderRow>> {
        let oid = client_order_id
            .and_then(|id| self.dispatch.venue_order_id(&id))
            .or(venue_order_id)
            .and_then(|id| id.as_str().parse::<u64>().ok());
        let cloid = client_order_id.map(|id| id.to_string());
        if oid.is_none() && cloid.is_none() {
            return Ok(None);
        }
        if let Some(oid) = oid {
            match self.http.request_order_details(oid).await {
                Ok(row) => return Ok(Some(row)),
                // A trigger order is not in the book: the lists below may know it
                Err(TxflowHttpError::Venue { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let is_it = |order: &TxflowOrder| {
            Some(order.oid) == oid || (cloid.is_some() && order.cloid == cloid)
        };
        let open = self.open_rows().await?;
        if let Some(row) = open.into_iter().find(|row| is_it(&row.order)) {
            return Ok(Some(row));
        }
        let history = self
            .http
            .request_historical_orders(&self.user, Some(self.since_ms()), None)
            .await?;
        Ok(history.into_iter().find(|row| is_it(&row.order)))
    }

    async fn fill_reports(
        &self,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
    ) -> anyhow::Result<Vec<FillReport>> {
        let fills = self
            .http
            .request_user_fills(&self.user, start_ms, end_ms)
            .await?;
        Ok(parse_fill_reports(
            fills,
            self.account_id,
            |key| self.instruments.lookup(key),
            now(),
        ))
    }

    async fn position_reports(
        &self,
        instrument_id: Option<InstrumentId>,
    ) -> anyhow::Result<Vec<PositionStatusReport>> {
        let state = self.http.request_clearinghouse_state(&self.user).await?;
        let ts_init = now();
        let mut reports = Vec::new();
        for entry in &state.asset_positions {
            let position = &entry.position;
            let Some(instrument) = self.instruments.lookup(&position.instrument_key()) else {
                log::warn!("Position on unknown market {}", position.coin);
                continue;
            };
            if instrument_id.is_none_or(|id| id == instrument.id()) {
                reports.push(parse_position_status_report(
                    position,
                    self.account_id,
                    &instrument,
                    ts_init,
                )?);
            }
        }
        // The venue lists open positions only: no row for the asked market means flat
        if reports.is_empty()
            && let Some(instrument) = instrument_id.and_then(|id| self.instruments.get(&id))
        {
            let flat = Position {
                coin: String::new(),
                asset: None,
                szi: None,
                entry_px: None,
            };
            reports.push(parse_position_status_report(
                &flat,
                self.account_id,
                &instrument,
                ts_init,
            )?);
        }
        Ok(reports)
    }

    /// After a reconnect: what the order stream did not deliver while the socket was down.
    /// Open orders come again with the snapshot of the new subscription.
    async fn catch_up(&self) {
        // shortcut: the window starts at connect, so it grows with the session; start it at
        // the last disconnect when a session runs for weeks.
        match self
            .http
            .request_historical_orders(&self.user, Some(self.since_ms()), None)
            .await
        {
            Ok(rows) => {
                for row in &rows {
                    self.dispatch.handle_row(row, now());
                }
            }
            Err(e) => log::error!("TxFlow order catch-up failed: {e}"),
        }
        if let Err(e) = self.refresh_account().await {
            log::warn!("TxFlow account refresh failed: {e}");
        }
    }
}

async fn consume(
    shared: Arc<Shared>,
    mut ws: TxflowWebSocketClient,
    mut rx: UnboundedReceiver<WsMessage>,
    cancel: CancellationToken,
) {
    loop {
        let message = tokio::select! {
            () = cancel.cancelled() => break,
            message = rx.recv() => match message {
                Some(message) => message,
                None => break,
            },
        };
        match message {
            WsMessage::OrderUpdates(rows) => {
                for row in &rows {
                    shared.dispatch.handle_row(row, now());
                }
            }
            WsMessage::ClearinghouseState(state) => shared.emit_balance(&state),
            WsMessage::Reconnected => shared.catch_up().await,
            _ => {}
        }
    }
    ws.disconnect().await;
}

/// Execution client for one TxFlow account.
#[derive(Debug)]
pub struct TxflowExecutionClient {
    core: ExecutionClientCore,
    config: TxflowExecClientConfig,
    emitter: ExecutionEventEmitter,
    http: TxflowHttpClient,
    shared: Option<Arc<Shared>>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl TxflowExecutionClient {
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created.
    pub fn new(core: ExecutionClientCore, config: TxflowExecClientConfig) -> anyhow::Result<Self> {
        let http = TxflowHttpClient::new(
            config.environment,
            config.base_url_http.clone(),
            config.http_timeout_secs,
            config.proxy_url.clone(),
        )?;
        let emitter = ExecutionEventEmitter::new(
            get_atomic_clock_realtime(),
            core.trader_id,
            core.account_id,
            core.account_type,
            None,
        );
        Ok(Self {
            core,
            config,
            emitter,
            http,
            shared: None,
            cancel: CancellationToken::new(),
            tasks: Vec::new(),
        })
    }

    fn shared(&self) -> anyhow::Result<Arc<Shared>> {
        self.shared
            .clone()
            .filter(|_| self.is_connected())
            .context("TxFlow execution client is not connected")
    }

    fn request(
        &self,
        shared: &Shared,
        order: &OrderAny,
        slippage_bps: u32,
    ) -> anyhow::Result<(Grouping, OrderBody)> {
        let instrument_id = order.instrument_id();
        let instrument = shared
            .instruments
            .get(&instrument_id)
            .with_context(|| format!("unknown instrument {instrument_id}"))?;
        let asset = shared
            .instruments
            .asset(&instrument_id)
            .with_context(|| format!("unknown instrument {instrument_id}"))?;
        order_body(
            order,
            asset,
            &instrument,
            self.core.cache().quote(&instrument_id),
            slippage_bps,
            self.config.margin_mode,
        )
    }

    fn submit(&self, orders: Vec<OrderAny>, params: Option<&Params>) -> anyhow::Result<()> {
        let shared = self.shared()?;
        let slippage_bps = params
            .and_then(|params| params.get_u64("market_order_slippage_bps"))
            .and_then(|bps| u32::try_from(bps).ok())
            .unwrap_or(self.config.market_order_slippage_bps);
        let mut plain = Vec::new();
        let mut triggers = Vec::new();
        for order in orders {
            if order.is_closed() {
                log::warn!("Cannot submit closed order {}", order.client_order_id());
                continue;
            }
            match self.request(&shared, &order, slippage_bps) {
                Ok((grouping, body)) => {
                    self.emitter.emit_order_submitted(&order);
                    shared.dispatch.track(&order);
                    let request = (order.client_order_id(), body);
                    match grouping {
                        Grouping::Na => plain.push(request),
                        Grouping::NormalTrigger => triggers.push(request),
                    }
                }
                Err(e) => self.emitter.emit_order_denied(&order, &e.to_string()),
            }
        }
        spawn(async move {
            shared.submit(Grouping::Na, &plain).await;
            // One action each: the grouping reads a batch of trigger orders as one group
            for request in triggers {
                shared.submit(Grouping::NormalTrigger, &[request]).await;
            }
        });
        Ok(())
    }

    fn modify(&self, shared: &Shared, cmd: &ModifyOrder) -> anyhow::Result<Modify> {
        let order = self.core.get_order(&cmd.client_order_id)?;
        // The venue cannot move a trigger price
        anyhow::ensure!(
            order.order_type() == OrderType::Limit && cmd.trigger_price.is_none(),
            "only price and quantity of a limit order can be modified"
        );
        let instrument = shared
            .instruments
            .get(&cmd.instrument_id)
            .with_context(|| format!("unknown instrument {}", cmd.instrument_id))?;
        let venue_order_id = shared
            .dispatch
            .venue_order_id(&cmd.client_order_id)
            .or(cmd.venue_order_id)
            .or(order.venue_order_id())
            .context("the order has no venue order id yet")?;
        let quantity = cmd.quantity.unwrap_or(order.quantity());
        let price = cmd.price.or(order.price()).context("order without price")?;
        let body = ModifyBody {
            o: venue_order_id
                .as_str()
                .parse()
                .context("venue order id is not a number")?,
            p: wire_decimal(
                price.as_decimal(),
                instrument.price_increment().as_decimal(),
            )?,
            // The new total size of the order, filled part included
            s: wire_decimal(
                quantity.as_decimal(),
                instrument.size_increment().as_decimal(),
            )?,
        };
        Ok(Modify {
            order,
            venue_order_id,
            quantity,
            price,
            body,
        })
    }

    fn cancel_of(
        &self,
        strategy_id: StrategyId,
        instrument_id: InstrumentId,
        client_order_id: ClientOrderId,
        venue_order_id: Option<VenueOrderId>,
    ) -> Cancel {
        let trigger = self
            .core
            .get_order(&client_order_id)
            .is_ok_and(|order| is_trigger(order.order_type()));
        Cancel {
            strategy_id,
            instrument_id,
            client_order_id,
            venue_order_id,
            trigger,
        }
    }

    fn cancel_cmd(&self, cmd: &CancelOrder) -> Cancel {
        self.cancel_of(
            cmd.strategy_id,
            cmd.instrument_id,
            cmd.client_order_id,
            cmd.venue_order_id,
        )
    }
}

#[async_trait(?Send)]
impl ExecutionClient for TxflowExecutionClient {
    fn is_connected(&self) -> bool {
        self.core.is_connected() && !self.cancel.is_cancelled()
    }

    fn client_id(&self) -> ClientId {
        self.core.client_id
    }

    fn account_id(&self) -> AccountId {
        self.core.account_id
    }

    fn venue(&self) -> Venue {
        self.core.venue
    }

    fn oms_type(&self) -> OmsType {
        self.core.oms_type
    }

    fn get_account(&self) -> Option<AccountAny> {
        self.core.cache().account_owned(&self.core.account_id)
    }

    fn generate_account_state(
        &self,
        balances: Vec<AccountBalance>,
        margins: Vec<MarginBalance>,
        reported: bool,
        ts_event: UnixNanos,
        info: Option<Params>,
    ) -> anyhow::Result<()> {
        self.emitter
            .emit_account_state(balances, margins, reported, ts_event, info);
        Ok(())
    }

    fn start(&mut self) -> anyhow::Result<()> {
        if !self.core.is_started() {
            self.emitter.set_sender(get_exec_event_sender());
            self.core.set_started();
        }
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        self.core.set_stopped();
        Ok(())
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        if self.is_connected() {
            return Ok(());
        }
        let config = &self.config;
        let (address_var, key_var) = match config.environment {
            TxflowEnvironment::Mainnet => (ENV_ACCOUNT_ADDRESS, ENV_AGENT_PRIVATE_KEY),
            TxflowEnvironment::Testnet => {
                (ENV_TESTNET_ACCOUNT_ADDRESS, ENV_TESTNET_AGENT_PRIVATE_KEY)
            }
        };
        let address = get_or_env_var_opt(config.account_address.clone(), address_var)
            .with_context(|| {
                format!("TxFlow execution needs an account address ({address_var})")
            })?;
        let key = get_or_env_var_opt(config.agent_private_key.clone(), key_var)
            .with_context(|| format!("TxFlow execution needs an agent key ({key_var})"))?;
        let address =
            Address::from_str(address.trim()).context("TxFlow account address is not valid")?;
        let signer = TxflowSigner::new(&key, config.environment)?;
        if signer.address() == address {
            log::warn!(
                "The TxFlow signing key is the account key: use an agent key, which cannot \
                 withdraw"
            );
        }

        self.http.request_instruments().await?;
        let shared = Arc::new(Shared {
            http: self.http.clone(),
            signer,
            nonces: NonceManager::default(),
            dispatch: OrderDispatch::new(
                self.core.account_id,
                self.emitter.clone(),
                self.http.instruments().clone(),
            ),
            emitter: self.emitter.clone(),
            instruments: self.http.instruments().clone(),
            account_id: self.core.account_id,
            user: format!("{address:#x}"),
            connected_ms: now().as_u64() / 1_000_000,
        });
        for (instrument_id, leverage) in &config.leverage {
            shared
                .update_leverage(instrument_id, *leverage)
                .await
                .with_context(|| format!("setting the leverage of {instrument_id}"))?;
        }
        shared.refresh_account().await?;

        let (node_url, _) = ws_urls(config.environment, config.base_url_http.as_deref());
        let mut ws = TxflowWebSocketClient::node(node_url);
        for channel in ["orderUpdates", "clearinghouseState"] {
            ws.subscribe(json!({"type": channel, "user": shared.user}))
                .await?;
        }
        let rx = ws.connect().await?;

        self.cancel = CancellationToken::new();
        self.tasks = vec![spawn(consume(shared.clone(), ws, rx, self.cancel.clone()))];
        self.shared = Some(shared);
        self.core.set_connected();
        log::info!("Connected TxFlow execution client {}", self.core.client_id);
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        for mut task in self.tasks.drain(..) {
            if tokio::time::timeout(DISCONNECT_TIMEOUT, &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
        self.shared = None;
        self.core.set_disconnected();
        Ok(())
    }

    fn submit_order(&self, cmd: SubmitOrder) -> anyhow::Result<()> {
        let order = self.core.get_order(&cmd.client_order_id)?;
        self.submit(vec![order], cmd.params.as_ref())
    }

    fn submit_order_list(&self, cmd: SubmitOrderList) -> anyhow::Result<()> {
        let orders = self.core.get_orders_for_list(&cmd.order_list)?;
        self.submit(orders, cmd.params.as_ref())
    }

    fn modify_order(&self, cmd: ModifyOrder) -> anyhow::Result<()> {
        let shared = self.shared()?;
        match self.modify(&shared, &cmd) {
            Ok(modify) => {
                spawn(async move { shared.modify(modify).await });
            }
            Err(e) => self.emitter.emit_order_modify_rejected_event(
                cmd.strategy_id,
                cmd.instrument_id,
                cmd.client_order_id,
                cmd.venue_order_id,
                &e.to_string(),
                now(),
            ),
        }
        Ok(())
    }

    fn cancel_order(&self, cmd: CancelOrder) -> anyhow::Result<()> {
        let shared = self.shared()?;
        let cancels = vec![self.cancel_cmd(&cmd)];
        spawn(async move { shared.cancel(cancels).await });
        Ok(())
    }

    fn cancel_all_orders(&self, cmd: CancelAllOrders) -> anyhow::Result<()> {
        let shared = self.shared()?;
        // The venue has no cancel-all action: the open orders of the cache go in one batch
        let cancels: Vec<Cancel> = self
            .core
            .cache()
            .orders_open(
                None,
                Some(&cmd.instrument_id),
                Some(&cmd.strategy_id),
                None,
                cmd.order_side,
            )
            .iter()
            .map(|order| Cancel {
                strategy_id: order.strategy_id(),
                instrument_id: order.instrument_id(),
                client_order_id: order.client_order_id(),
                venue_order_id: order.venue_order_id(),
                trigger: is_trigger(order.order_type()),
            })
            .collect();
        spawn(async move { shared.cancel(cancels).await });
        Ok(())
    }

    fn batch_cancel_orders(&self, cmd: BatchCancelOrders) -> anyhow::Result<()> {
        let shared = self.shared()?;
        let cancels = cmd
            .cancels
            .iter()
            .map(|cancel| self.cancel_cmd(cancel))
            .collect();
        spawn(async move { shared.cancel(cancels).await });
        Ok(())
    }

    fn query_account(&self, _cmd: QueryAccount) -> anyhow::Result<()> {
        let shared = self.shared()?;
        spawn(async move {
            if let Err(e) = shared.refresh_account().await {
                log::warn!("TxFlow account query failed: {e}");
            }
        });
        Ok(())
    }

    fn query_order(&self, cmd: QueryOrder) -> anyhow::Result<()> {
        let shared = self.shared()?;
        spawn(async move {
            match shared
                .order_row(Some(cmd.client_order_id), cmd.venue_order_id)
                .await
            {
                // Events for an order of this session, a report for any other
                Ok(Some(row)) => shared.dispatch.handle_row(&row, now()),
                Ok(None) => log::warn!("TxFlow order {} not found", cmd.client_order_id),
                Err(e) => log::warn!("TxFlow query of order {} failed: {e}", cmd.client_order_id),
            }
        });
        Ok(())
    }

    async fn generate_order_status_report(
        &self,
        cmd: &GenerateOrderStatusReport,
    ) -> anyhow::Result<Option<OrderStatusReport>> {
        let shared = self.shared()?;
        let row = shared
            .order_row(cmd.client_order_id, cmd.venue_order_id)
            .await?;
        Ok(row.and_then(|row| shared.report(&row)))
    }

    async fn generate_order_status_reports(
        &self,
        cmd: &GenerateOrderStatusReports,
    ) -> anyhow::Result<Vec<OrderStatusReport>> {
        let shared = self.shared()?;
        let millis = |ts: UnixNanos| ts.as_u64() / 1_000_000;
        let mut reports = shared
            .order_reports(cmd.open_only, cmd.start.map(millis), cmd.end.map(millis))
            .await?;
        if let Some(instrument_id) = cmd.instrument_id {
            reports.retain(|report| report.instrument_id == instrument_id);
        }
        Ok(reports)
    }

    async fn generate_fill_reports(
        &self,
        cmd: GenerateFillReports,
    ) -> anyhow::Result<Vec<FillReport>> {
        let shared = self.shared()?;
        let millis = |ts: UnixNanos| ts.as_u64() / 1_000_000;
        let mut reports = shared
            .fill_reports(cmd.start.map(millis), cmd.end.map(millis))
            .await?;
        reports.retain(|report| {
            cmd.instrument_id
                .is_none_or(|id| report.instrument_id == id)
                && cmd
                    .venue_order_id
                    .is_none_or(|id| report.venue_order_id == id)
        });
        Ok(reports)
    }

    async fn generate_position_status_reports(
        &self,
        cmd: &GeneratePositionStatusReports,
    ) -> anyhow::Result<Vec<PositionStatusReport>> {
        self.shared()?.position_reports(cmd.instrument_id).await
    }
}
