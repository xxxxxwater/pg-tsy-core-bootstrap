use std::{str::FromStr, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use pg_execution::{
    ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck, VenueOrderSnapshot,
    VenueOrderState, VenuePositionSnapshot,
};
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::{prelude::ToPrimitive, Decimal};

use crate::{sdk, IbkrConfig, IbkrStockSpec};
use sdk::accounts::PositionUpdate;
use sdk::orders::{
    Action, CancelOrder, ExecutionFilter, ExecutionSide, Executions, OrderData, OrderStatusKind,
    Orders, PlaceOrder,
};
use sdk::subscriptions::{SubscriptionItem, SubscriptionItemStreamExt};

#[derive(Debug, Clone)]
pub struct IbkrExecutionConfig {
    pub ibkr: IbkrConfig,
    pub instrument: IbkrStockSpec,
    /// Time to wait for the first order/execution frame proving the order exists.
    pub submit_ack_timeout_ms: u64,
    /// Time to wait for a venue cancellation confirmation before reconciliation.
    pub cancel_ack_timeout_ms: u64,
    /// IBKR does not provide a crypto-style atomic reduce-only flag for ordinary
    /// stock orders. When false, reduce-only intents are rejected. When true,
    /// the adapter reconciles the current account position immediately before
    /// submission and rejects any order that could cross through flat.
    pub allow_software_reduce_only: bool,
}

impl IbkrExecutionConfig {
    pub fn validate(&self) -> Result<(), ExecutionError> {
        if self.ibkr.gateway_addr.trim().is_empty() {
            return Err(ExecutionError::Conversion(
                "IBKR gateway_addr cannot be empty".into(),
            ));
        }
        if self.instrument.symbol.trim().is_empty()
            || self.instrument.exchange.trim().is_empty()
            || self.instrument.currency.trim().is_empty()
        {
            return Err(ExecutionError::Conversion(
                "IBKR instrument symbol/exchange/currency cannot be empty".into(),
            ));
        }
        for (name, value) in [
            ("submit_ack_timeout_ms", self.submit_ack_timeout_ms),
            ("cancel_ack_timeout_ms", self.cancel_ack_timeout_ms),
        ] {
            if value == 0 || value > 60_000 {
                return Err(ExecutionError::Conversion(format!(
                    "IBKR {name} must be in [1, 60000]"
                )));
            }
        }
        Ok(())
    }
}

pub struct IbkrExecutionAdapter {
    config: IbkrExecutionConfig,
    client: Arc<sdk::Client>,
}

impl IbkrExecutionAdapter {
    pub async fn connect(config: IbkrExecutionConfig) -> Result<Self, ExecutionError> {
        config.validate()?;
        let client = sdk::Client::connect(&config.ibkr.gateway_addr, config.ibkr.client_id)
            .await
            .map_err(map_connect_error)?;
        Ok(Self {
            config,
            client: Arc::new(client),
        })
    }

    fn contract(&self) -> sdk::contracts::Contract {
        let mut builder = sdk::contracts::Contract::stock(&self.config.instrument.symbol)
            .on_exchange(&self.config.instrument.exchange)
            .in_currency(&self.config.instrument.currency);
        if let Some(primary) = &self.config.instrument.primary_exchange {
            builder = builder.primary(primary);
        }
        let mut contract = builder.build();
        if let Some(con_id) = self.config.instrument.con_id {
            contract.contract_id = con_id;
        }
        contract
    }

    fn validate_intent(&self, intent: &OrderIntent) -> Result<(), ExecutionError> {
        if intent.venue != Venue::InteractiveBrokers {
            return Err(ExecutionError::Unsupported(
                "IBKR adapter received a non-IBKR intent".into(),
            ));
        }
        if intent.asset != self.config.instrument.symbol {
            return Err(ExecutionError::Unsupported(format!(
                "IBKR execution asset {} does not match configured instrument {}",
                intent.asset, self.config.instrument.symbol
            )));
        }
        if intent.quantity <= Decimal::ZERO {
            return Err(ExecutionError::Rejected(
                "IBKR order quantity must be positive".into(),
            ));
        }
        Ok(())
    }

    async fn build_order(
        &self,
        intent: &OrderIntent,
    ) -> Result<sdk::orders::Order, ExecutionError> {
        self.validate_intent(intent)?;
        if intent.effect == ExposureEffect::ReduceOnly {
            if !self.config.allow_software_reduce_only {
                return Err(ExecutionError::Unsupported(
                    "IBKR has no atomic reduce-only guarantee in this adapter; enable allow_software_reduce_only explicitly to use the position-checked software guard"
                        .into(),
                ));
            }
            let current = self.current_instrument_position().await?;
            validate_software_reduce_only(intent.side, intent.quantity, current)?;
        }

        let quantity = exact_f64(intent.quantity, "quantity")?;
        let limit_price = intent
            .limit_price
            .map(|price| exact_f64(price, "limit price"))
            .transpose()?;
        if matches!(limit_price, Some(value) if value <= 0.0) {
            return Err(ExecutionError::Rejected(
                "IBKR limit price must be positive".into(),
            ));
        }

        let mut order = sdk::orders::Order {
            action: match intent.side {
                Side::Buy => Action::Buy,
                Side::Sell => Action::Sell,
            },
            total_quantity: quantity,
            order_type: if limit_price.is_some() {
                "LMT".to_string()
            } else {
                "MKT".to_string()
            },
            limit_price,
            order_ref: intent.client_order_id(),
            ..sdk::orders::Order::default()
        };
        if let Some(account) = &self.config.ibkr.account {
            order.account = account.clone();
        }
        Ok(order)
    }

    fn account_matches(&self, account: &str) -> bool {
        self.config
            .ibkr
            .account
            .as_deref()
            .is_none_or(|expected| expected == account)
    }

    fn contract_matches(&self, contract: &sdk::contracts::Contract) -> bool {
        if let Some(con_id) = self.config.instrument.con_id {
            contract.contract_id == con_id
        } else {
            contract.symbol.to_string() == self.config.instrument.symbol
        }
    }

    async fn position_rows(&self) -> Result<Vec<sdk::accounts::Position>, ExecutionError> {
        let subscription = self.client.positions().await.map_err(map_read_error)?;
        let mut stream = subscription.filter_data();
        let mut positions = Vec::new();
        while let Some(item) = stream.next().await {
            match item.map_err(map_read_error)? {
                PositionUpdate::Position(position) => positions.push(position),
                PositionUpdate::PositionEnd => break,
            }
        }
        Ok(positions)
    }

    async fn current_instrument_position(&self) -> Result<Decimal, ExecutionError> {
        let mut total = Decimal::ZERO;
        for position in self.position_rows().await? {
            if self.account_matches(&position.account) && self.contract_matches(&position.contract) {
                total += decimal_from_f64(position.position, "position")?;
            }
        }
        Ok(total)
    }

    async fn open_order_rows(&self) -> Result<Vec<OrderData>, ExecutionError> {
        let subscription = self.client.open_orders().await.map_err(map_read_error)?;
        collect_order_data(subscription).await
    }

    async fn completed_order_rows(&self) -> Result<Vec<OrderData>, ExecutionError> {
        let subscription = self
            .client
            .completed_orders(true)
            .await
            .map_err(map_read_error)?;
        collect_order_data(subscription).await
    }

    async fn execution_rows(&self) -> Result<Vec<sdk::orders::ExecutionData>, ExecutionError> {
        let filter = ExecutionFilter {
            client_id: Some(self.config.ibkr.client_id),
            account_code: self.config.ibkr.account.clone().unwrap_or_default(),
            symbol: self.config.instrument.symbol.clone(),
            ..ExecutionFilter::default()
        };
        let subscription = self.client.executions(filter).await.map_err(map_read_error)?;
        let mut stream = subscription.filter_data();
        let mut executions = Vec::new();
        while let Some(item) = stream.next().await {
            match item.map_err(map_read_error)? {
                Executions::ExecutionData(execution) => executions.push(execution),
                Executions::CommissionReport(_) => {}
            }
        }
        Ok(executions)
    }

    async fn lookup_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        for order in self.open_order_rows().await? {
            if order.order.order_ref == client_order_id {
                return map_order_data(order).map(Some);
            }
        }

        for order in self.completed_order_rows().await? {
            if order.order.order_ref == client_order_id {
                return map_order_data(order).map(Some);
            }
        }

        // A fast market order can disappear from open orders before recovery runs.
        // Executions retain order_reference and API order_id, so they are the final
        // proof that an ambiguous placeOrder was accepted and must not be resubmitted.
        for execution in self.execution_rows().await? {
            if execution.execution.order_reference == client_order_id {
                return map_execution_proof(execution).map(Some);
            }
        }
        Ok(None)
    }

    async fn recover_submit(
        &self,
        client_order_id: &str,
        cause: impl Into<String>,
    ) -> Result<VenueOrderAck, ExecutionError> {
        let cause = cause.into();
        if let Some(order) = self.lookup_by_client_id(client_order_id).await? {
            return Ok(VenueOrderAck {
                venue_order_id: order.venue_order_id,
                client_order_id: client_order_id.to_string(),
            });
        }
        Err(ExecutionError::Unknown(format!(
            "IBKR submit outcome unresolved for {client_order_id}; do not place a replacement order until open/completed/execution reconciliation proves absence: {cause}"
        )))
    }

    async fn await_submit_ack(
        &self,
        mut subscription: sdk::subscriptions::Subscription<PlaceOrder>,
        order_id: i32,
        client_order_id: &str,
    ) -> Result<VenueOrderAck, ExecutionError> {
        let timeout = Duration::from_millis(self.config.submit_ack_timeout_ms);
        let outcome = tokio::time::timeout(timeout, async {
            while let Some(item) = subscription.next().await {
                match item {
                    Ok(SubscriptionItem::Data(PlaceOrder::OpenOrder(order)))
                        if order.order.order_ref == client_order_id =>
                    {
                        return Ok(Some(order.order_id));
                    }
                    Ok(SubscriptionItem::Data(PlaceOrder::OrderStatus(status)))
                        if status.order_id == order_id =>
                    {
                        return Ok(Some(status.order_id));
                    }
                    Ok(SubscriptionItem::Data(PlaceOrder::ExecutionData(execution)))
                        if execution.execution.order_reference == client_order_id =>
                    {
                        return Ok(Some(execution.execution.order_id));
                    }
                    Ok(SubscriptionItem::Notice(notice)) if notice.is_order_rejection() => {
                        return Err(ExecutionError::Rejected(notice.to_string()));
                    }
                    Ok(SubscriptionItem::Data(PlaceOrder::CommissionReport(_)))
                    | Ok(SubscriptionItem::Notice(_)) => {}
                    Err(error) => return Err(map_ambiguous_submit_error(error)),
                    _ => {}
                }
            }
            Ok(None)
        })
        .await;

        match outcome {
            Ok(Ok(Some(venue_order_id))) => Ok(VenueOrderAck {
                venue_order_id: venue_order_id.to_string(),
                client_order_id: client_order_id.to_string(),
            }),
            Ok(Err(ExecutionError::Rejected(reason))) => Err(ExecutionError::Rejected(reason)),
            Ok(Err(error)) => self.recover_submit(client_order_id, error.to_string()).await,
            Ok(Ok(None)) => {
                self.recover_submit(
                    client_order_id,
                    "IBKR order subscription ended before acknowledgment",
                )
                .await
            }
            Err(_) => {
                self.recover_submit(client_order_id, "IBKR acknowledgment timeout")
                    .await
            }
        }
    }

    async fn await_cancel_ack(
        &self,
        mut subscription: sdk::subscriptions::Subscription<CancelOrder>,
        order_id: i32,
        client_order_id: &str,
    ) -> Result<(), ExecutionError> {
        let timeout = Duration::from_millis(self.config.cancel_ack_timeout_ms);
        let outcome = tokio::time::timeout(timeout, async {
            while let Some(item) = subscription.next().await {
                match item {
                    Ok(SubscriptionItem::Notice(notice)) if notice.is_cancellation() => {
                        return Ok(true)
                    }
                    Ok(SubscriptionItem::Data(CancelOrder::OrderStatus(status)))
                        if status.order_id == order_id
                            && matches!(
                                status.status,
                                OrderStatusKind::Cancelled | OrderStatusKind::ApiCancelled
                            ) =>
                    {
                        return Ok(true)
                    }
                    Ok(SubscriptionItem::Notice(notice)) if notice.is_order_rejection() => {
                        return Err(ExecutionError::Rejected(notice.to_string()));
                    }
                    Ok(_) => {}
                    Err(error) => return Err(map_ambiguous_submit_error(error)),
                }
            }
            Ok(false)
        })
        .await;

        match outcome {
            Ok(Ok(true)) => Ok(()),
            Ok(Err(ExecutionError::Rejected(reason))) => Err(ExecutionError::Rejected(reason)),
            Ok(Err(error)) => Err(ExecutionError::Unknown(format!(
                "IBKR cancel outcome is ambiguous for {client_order_id}; reconcile before retry: {error}"
            ))),
            Ok(Ok(false)) | Err(_) => match self.lookup_by_client_id(client_order_id).await? {
                Some(snapshot)
                    if matches!(
                        snapshot.state,
                        VenueOrderState::Canceled | VenueOrderState::Filled
                    ) => Ok(()),
                Some(snapshot) => Err(ExecutionError::Unknown(format!(
                    "IBKR cancel outcome unresolved for {client_order_id}; reconciled state is {:?}",
                    snapshot.state
                ))),
                None => Ok(()),
            },
        }
    }
}

#[async_trait]
impl ExecutionAdapter for IbkrExecutionAdapter {
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        let client_order_id = intent.client_order_id();

        // Replaying a persisted intent after restart is safe: order_ref is stable
        // and we reconcile it before any new placeOrder message is emitted.
        if let Some(existing) = self.lookup_by_client_id(&client_order_id).await? {
            return Ok(VenueOrderAck {
                venue_order_id: existing.venue_order_id,
                client_order_id,
            });
        }

        let order = self.build_order(intent).await?;
        let contract = self.contract();
        let order_id = self.client.next_order_id();
        match self.client.place_order(order_id, &contract, &order).await {
            Ok(subscription) => {
                self.await_submit_ack(subscription, order_id, &client_order_id)
                    .await
            }
            Err(error) if is_definite_pre_submit_error(&error) => Err(map_pre_submit_error(error)),
            Err(error) => self.recover_submit(&client_order_id, error.to_string()).await,
        }
    }

    async fn cancel(&self, order: OrderLocator<'_>) -> Result<(), ExecutionError> {
        let venue_order_id = if let Some(value) = order.venue_order_id {
            value.to_string()
        } else {
            let existing = self
                .lookup_by_client_id(order.client_order_id)
                .await?
                .ok_or_else(|| {
                    ExecutionError::Unknown(format!(
                        "IBKR order {} not found; cannot prove which live order to cancel",
                        order.client_order_id
                    ))
                })?;
            existing.venue_order_id
        };
        let order_id = parse_cancelable_order_id(&venue_order_id)?;
        match self.client.cancel_order(order_id, "").await {
            Ok(subscription) => {
                self.await_cancel_ack(subscription, order_id, order.client_order_id)
                    .await
            }
            Err(error) if is_definite_pre_submit_error(&error) => Err(map_pre_submit_error(error)),
            Err(error) => Err(ExecutionError::Unknown(format!(
                "IBKR cancel outcome is ambiguous; reconcile before retry: {error}"
            ))),
        }
    }

    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        self.open_order_rows()
            .await?
            .into_iter()
            .map(map_order_data)
            .collect()
    }

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        self.position_rows()
            .await?
            .into_iter()
            .filter(|position| self.account_matches(&position.account))
            .map(|position| {
                Ok(VenuePositionSnapshot {
                    asset: position.contract.symbol.to_string(),
                    quantity: decimal_from_f64(position.position, "position")?,
                })
            })
            .collect()
    }

    async fn find_order_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        self.lookup_by_client_id(client_order_id).await
    }
}

async fn collect_order_data(
    subscription: sdk::subscriptions::Subscription<Orders>,
) -> Result<Vec<OrderData>, ExecutionError> {
    let mut stream = subscription.filter_data();
    let mut orders = Vec::new();
    while let Some(item) = stream.next().await {
        match item.map_err(map_read_error)? {
            Orders::OrderData(order) => orders.push(order),
            Orders::OrderStatus(_) => {}
        }
    }
    Ok(orders)
}

fn map_order_data(order: OrderData) -> Result<VenueOrderSnapshot, ExecutionError> {
    let requested = decimal_from_f64(order.order.total_quantity, "order quantity")?;
    let reported_filled = decimal_from_f64(order.order.filled_quantity, "filled quantity")?;
    let state = map_order_state(
        &order.order_state.status,
        reported_filled,
        requested,
        &order.order_state.reject_reason,
    );
    let filled = if state == VenueOrderState::Filled {
        requested
    } else {
        reported_filled.min(requested)
    };
    let venue_order_id = if order.order_id >= 0 {
        order.order_id.to_string()
    } else if order.order.perm_id != 0 {
        format!("perm:{}", order.order.perm_id)
    } else {
        format!("completed:{}", order.order.order_ref)
    };
    Ok(VenueOrderSnapshot {
        venue_order_id,
        client_order_id: (!order.order.order_ref.is_empty()).then_some(order.order.order_ref),
        asset: order.contract.symbol.to_string(),
        side: map_action(order.order.action),
        requested_quantity: requested,
        filled_quantity: filled,
        limit_price: order
            .order
            .limit_price
            .map(|price| decimal_from_f64(price, "limit price"))
            .transpose()?,
        state,
    })
}

fn map_execution_proof(
    execution: sdk::orders::ExecutionData,
) -> Result<VenueOrderSnapshot, ExecutionError> {
    let quantity = decimal_from_f64(execution.execution.cumulative_quantity, "execution quantity")?;
    Ok(VenueOrderSnapshot {
        venue_order_id: execution.execution.order_id.to_string(),
        client_order_id: (!execution.execution.order_reference.is_empty())
            .then_some(execution.execution.order_reference),
        asset: execution.contract.symbol.to_string(),
        side: match execution.execution.side {
            ExecutionSide::Bought => Side::Buy,
            ExecutionSide::Sold => Side::Sell,
        },
        requested_quantity: quantity,
        filled_quantity: quantity,
        limit_price: None,
        // This snapshot is existence proof only. Without the original OrderData
        // we cannot infer whether cumulative_quantity equals the requested size.
        state: VenueOrderState::Unknown,
    })
}

fn map_action(action: Action) -> Side {
    match action {
        Action::Buy | Action::SellLong => Side::Buy,
        Action::Sell | Action::SellShort => Side::Sell,
    }
}

fn map_order_state(
    status: &OrderStatusKind,
    filled: Decimal,
    requested: Decimal,
    reject_reason: &str,
) -> VenueOrderState {
    match status {
        OrderStatusKind::Filled => VenueOrderState::Filled,
        OrderStatusKind::Cancelled | OrderStatusKind::ApiCancelled => VenueOrderState::Canceled,
        OrderStatusKind::Inactive if !reject_reason.trim().is_empty() => VenueOrderState::Rejected,
        OrderStatusKind::Inactive => VenueOrderState::Canceled,
        OrderStatusKind::Submitted
        | OrderStatusKind::PreSubmitted
        | OrderStatusKind::PendingSubmit
        | OrderStatusKind::PendingCancel => {
            if filled > Decimal::ZERO && filled < requested {
                VenueOrderState::PartiallyFilled
            } else {
                VenueOrderState::Open
            }
        }
        OrderStatusKind::ApiPending | OrderStatusKind::Unknown(_) => VenueOrderState::Unknown,
    }
}

fn validate_software_reduce_only(
    side: Side,
    quantity: Decimal,
    current: Decimal,
) -> Result<(), ExecutionError> {
    if current == Decimal::ZERO {
        return Err(ExecutionError::Rejected(
            "reduce-only rejected because the reconciled IBKR position is flat".into(),
        ));
    }
    let valid_direction = matches!((current.is_sign_positive(), side), (true, Side::Sell) | (false, Side::Buy));
    if !valid_direction {
        return Err(ExecutionError::Rejected(format!(
            "reduce-only side {side:?} would increase IBKR position {current}"
        )));
    }
    if quantity > current.abs() {
        return Err(ExecutionError::Rejected(format!(
            "reduce-only quantity {quantity} exceeds reconciled IBKR position {} and could cross through flat",
            current.abs()
        )));
    }
    Ok(())
}

fn exact_f64(value: Decimal, label: &str) -> Result<f64, ExecutionError> {
    let encoded = value.to_f64().ok_or_else(|| {
        ExecutionError::Conversion(format!("IBKR {label} cannot be represented as f64"))
    })?;
    if !encoded.is_finite() {
        return Err(ExecutionError::Conversion(format!(
            "IBKR {label} is non-finite"
        )));
    }
    Ok(encoded)
}

fn decimal_from_f64(value: f64, label: &str) -> Result<Decimal, ExecutionError> {
    if !value.is_finite() {
        return Err(ExecutionError::Conversion(format!(
            "IBKR {label} is non-finite"
        )));
    }
    Decimal::from_str(&value.to_string())
        .map_err(|error| ExecutionError::Conversion(error.to_string()))
}

fn parse_cancelable_order_id(value: &str) -> Result<i32, ExecutionError> {
    if value.starts_with("perm:") || value.starts_with("completed:") {
        return Err(ExecutionError::Unsupported(format!(
            "IBKR completed order identifier {value} is not cancelable"
        )));
    }
    value
        .parse::<i32>()
        .map_err(|error| ExecutionError::Conversion(format!("invalid IBKR order id: {error}")))
}

fn is_definite_pre_submit_error(error: &sdk::Error) -> bool {
    matches!(
        error,
        sdk::Error::InvalidArgument(_)
            | sdk::Error::ServerVersion(_, _, _)
            | sdk::Error::Parse(_, _, _)
            | sdk::Error::ParseInt(_)
            | sdk::Error::FromUtf8(_)
            | sdk::Error::UnsupportedTimeZone(_)
            | sdk::Error::NotImplemented
    )
}

fn map_pre_submit_error(error: sdk::Error) -> ExecutionError {
    match error {
        sdk::Error::InvalidArgument(message) => ExecutionError::Rejected(message),
        sdk::Error::ServerVersion(_, _, _)
        | sdk::Error::NotImplemented
        | sdk::Error::Parse(_, _, _)
        | sdk::Error::ParseInt(_)
        | sdk::Error::FromUtf8(_)
        | sdk::Error::UnsupportedTimeZone(_) => ExecutionError::Conversion(error.to_string()),
        sdk::Error::Notice(notice) if notice.is_order_rejection() => {
            ExecutionError::Rejected(notice.to_string())
        }
        other => ExecutionError::Unknown(other.to_string()),
    }
}

fn map_ambiguous_submit_error(error: sdk::Error) -> ExecutionError {
    if let sdk::Error::Notice(notice) = &error {
        if notice.is_order_rejection() {
            return ExecutionError::Rejected(notice.to_string());
        }
    }
    ExecutionError::Unknown(error.to_string())
}

fn map_connect_error(error: sdk::Error) -> ExecutionError {
    match error {
        sdk::Error::ConnectionRejected(message) => ExecutionError::Authentication(message),
        sdk::Error::ConnectionFailed
        | sdk::Error::ConnectionReset
        | sdk::Error::Io(_)
        | sdk::Error::InvalidFrame(_) => ExecutionError::Transport(error.to_string()),
        other => ExecutionError::Conversion(other.to_string()),
    }
}

fn map_read_error(error: sdk::Error) -> ExecutionError {
    match error {
        sdk::Error::ConnectionFailed
        | sdk::Error::ConnectionReset
        | sdk::Error::Io(_)
        | sdk::Error::InvalidFrame(_) => ExecutionError::Transport(error.to_string()),
        other => ExecutionError::Conversion(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_reduce_only_rejects_cross_through_flat() {
        let err = validate_software_reduce_only(
            Side::Sell,
            Decimal::from(11),
            Decimal::from(10),
        )
        .unwrap_err();
        assert!(matches!(err, ExecutionError::Rejected(_)));
    }

    #[test]
    fn software_reduce_only_accepts_long_reduction() {
        validate_software_reduce_only(
            Side::Sell,
            Decimal::from(4),
            Decimal::from(10),
        )
        .unwrap();
    }

    #[test]
    fn software_reduce_only_accepts_short_reduction() {
        validate_software_reduce_only(
            Side::Buy,
            Decimal::from(3),
            Decimal::from(-10),
        )
        .unwrap();
    }

    #[test]
    fn unknown_status_is_fail_closed() {
        assert_eq!(
            map_order_state(
                &OrderStatusKind::Unknown("FutureStatus".into()),
                Decimal::ZERO,
                Decimal::from(1),
                ""
            ),
            VenueOrderState::Unknown
        );
    }
}
