use std::{collections::HashMap, str::FromStr};

use alloy::{
    primitives::Address,
    signers::local::PrivateKeySigner,
};
use async_trait::async_trait;
use pg_execution::{
    ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck, VenueOrderSnapshot,
    VenueOrderState, VenuePositionSnapshot,
};
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use uuid::Uuid;

use crate::{sdk, HyperliquidNetwork};

#[derive(Debug, Clone)]
pub struct HyperliquidExecutionConfig {
    pub network: HyperliquidNetwork,
    pub account_address: String,
    /// Fractional slippage cap used only for market-style intents, e.g. 0.005 = 50 bps.
    pub max_market_slippage: f64,
}

impl HyperliquidExecutionConfig {
    pub fn validate(&self) -> Result<(), ExecutionError> {
        if !(0.0..=0.10).contains(&self.max_market_slippage) {
            return Err(ExecutionError::Conversion(
                "Hyperliquid max_market_slippage must be in [0, 0.10]".into(),
            ));
        }
        Address::from_str(&self.account_address)
            .map_err(|error| ExecutionError::Conversion(error.to_string()))?;
        Ok(())
    }
}

pub struct HyperliquidExecutionAdapter {
    config: HyperliquidExecutionConfig,
    account_address: Address,
    exchange: sdk::ExchangeClient,
    info: sdk::InfoClient,
}

impl HyperliquidExecutionAdapter {
    pub async fn connect(
        config: HyperliquidExecutionConfig,
        wallet: PrivateKeySigner,
    ) -> Result<Self, ExecutionError> {
        config.validate()?;
        let account_address = Address::from_str(&config.account_address)
            .map_err(|error| ExecutionError::Conversion(error.to_string()))?;
        let base_url = base_url(config.network);
        let exchange = sdk::ExchangeClient::new(None, wallet, Some(base_url), None, None)
            .await
            .map_err(map_connect_error)?;
        let info = sdk::InfoClient::new(None, Some(base_url(config.network)))
            .await
            .map_err(map_connect_error)?;
        Ok(Self {
            config,
            account_address,
            exchange,
            info,
        })
    }

    fn validate_intent(&self, intent: &OrderIntent) -> Result<(), ExecutionError> {
        if intent.venue != Venue::Hyperliquid {
            return Err(ExecutionError::Unsupported(
                "Hyperliquid adapter received a non-Hyperliquid intent".into(),
            ));
        }
        if intent.quantity <= Decimal::ZERO {
            return Err(ExecutionError::Rejected(
                "order quantity must be positive".into(),
            ));
        }
        if !self.exchange.coin_to_asset.contains_key(&intent.asset) {
            return Err(ExecutionError::Rejected(format!(
                "Hyperliquid asset not found: {}",
                intent.asset
            )));
        }
        Ok(())
    }

    fn client_request(&self, intent: &OrderIntent) -> Result<sdk::ClientOrderRequest, ExecutionError> {
        self.validate_intent(intent)?;
        let asset_meta = self
            .exchange
            .meta
            .universe
            .iter()
            .find(|asset| asset.name == intent.asset)
            .ok_or_else(|| {
                ExecutionError::Unsupported(format!(
                    "Hyperliquid spot execution is not yet enabled for {}",
                    intent.asset
                ))
            })?;
        let quantity = exact_size(intent.quantity, asset_meta.sz_decimals)?;
        let is_buy = intent.side == Side::Buy;
        let reduce_only = intent.effect == ExposureEffect::ReduceOnly;
        let asset_index = *self
            .exchange
            .coin_to_asset
            .get(&intent.asset)
            .ok_or_else(|| ExecutionError::Rejected("asset disappeared from metadata".into()))?;
        let max_decimals = if asset_index < 10_000 { 6 } else { 8 };
        let price_decimals = max_decimals.saturating_sub(asset_meta.sz_decimals);

        let (limit_px, tif) = match intent.limit_price {
            Some(limit_price) => {
                if limit_price <= Decimal::ZERO {
                    return Err(ExecutionError::Rejected(
                        "limit price must be positive".into(),
                    ));
                }
                let raw = limit_price.to_f64().ok_or_else(|| {
                    ExecutionError::Conversion("limit price cannot be represented as f64".into())
                })?;
                let normalized = round_to_significant_and_decimal(raw, 5, price_decimals);
                let normalized_decimal = Decimal::from_f64_retain(normalized).ok_or_else(|| {
                    ExecutionError::Conversion("normalized limit price is invalid".into())
                })?;
                if (normalized_decimal - limit_price).abs() > Decimal::new(1, 8) {
                    return Err(ExecutionError::Rejected(format!(
                        "limit price {} exceeds Hyperliquid price precision; normalized value would be {}",
                        limit_price, normalized
                    )));
                }
                (normalized, "Gtc".to_string())
            }
            None => {
                return Err(ExecutionError::Unsupported(
                    "market-style intent requires async price preparation".into(),
                ));
            }
        };

        Ok(sdk::ClientOrderRequest {
            asset: intent.asset.clone(),
            is_buy,
            reduce_only,
            limit_px,
            sz: quantity,
            cloid: Some(intent.intent_id),
            order_type: sdk::ClientOrder::Limit(sdk::ClientLimit { tif }),
        })
    }

    async fn market_request(
        &self,
        intent: &OrderIntent,
    ) -> Result<sdk::ClientOrderRequest, ExecutionError> {
        self.validate_intent(intent)?;
        let asset_meta = self
            .exchange
            .meta
            .universe
            .iter()
            .find(|asset| asset.name == intent.asset)
            .ok_or_else(|| {
                ExecutionError::Unsupported(format!(
                    "Hyperliquid spot execution is not yet enabled for {}",
                    intent.asset
                ))
            })?;
        let quantity = exact_size(intent.quantity, asset_meta.sz_decimals)?;
        let asset_index = *self
            .exchange
            .coin_to_asset
            .get(&intent.asset)
            .ok_or_else(|| ExecutionError::Rejected("asset disappeared from metadata".into()))?;
        let max_decimals = if asset_index < 10_000 { 6 } else { 8 };
        let price_decimals = max_decimals.saturating_sub(asset_meta.sz_decimals);
        let mids = self
            .info
            .all_mids()
            .await
            .map_err(map_read_error)?;
        let mid = mids
            .get(&intent.asset)
            .ok_or_else(|| ExecutionError::Unknown("Hyperliquid mid price unavailable".into()))?
            .parse::<f64>()
            .map_err(|error| ExecutionError::Conversion(error.to_string()))?;
        if !mid.is_finite() || mid <= 0.0 {
            return Err(ExecutionError::Conversion(
                "Hyperliquid returned an invalid mid price".into(),
            ));
        }
        let is_buy = intent.side == Side::Buy;
        let slippage_factor = if is_buy {
            1.0 + self.config.max_market_slippage
        } else {
            1.0 - self.config.max_market_slippage
        };
        let limit_px = round_to_significant_and_decimal(mid * slippage_factor, 5, price_decimals);

        Ok(sdk::ClientOrderRequest {
            asset: intent.asset.clone(),
            is_buy,
            reduce_only: intent.effect == ExposureEffect::ReduceOnly,
            limit_px,
            sz: quantity,
            cloid: Some(intent.intent_id),
            order_type: sdk::ClientOrder::Limit(sdk::ClientLimit {
                tif: "Ioc".to_string(),
            }),
        })
    }

    async fn request_for_intent(
        &self,
        intent: &OrderIntent,
    ) -> Result<sdk::ClientOrderRequest, ExecutionError> {
        if intent.limit_price.is_some() {
            self.client_request(intent)
        } else {
            self.market_request(intent).await
        }
    }

    async fn lookup_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        let venue_cloid = client_id_to_venue_cloid(client_order_id)?;
        let historical = self
            .info
            .historical_orders(self.account_address)
            .await
            .map_err(map_read_error)?;
        if let Some(order) = historical.into_iter().find(|entry| {
            entry.order.cloid.as_deref() == Some(venue_cloid.as_str())
        }) {
            return map_historical_order(order).map(Some);
        }

        let open = self
            .info
            .open_orders(self.account_address)
            .await
            .map_err(map_read_error)?;
        if let Some(order) = open
            .into_iter()
            .find(|entry| entry.cloid.as_deref() == Some(venue_cloid.as_str()))
        {
            return map_open_order(order, None).map(Some);
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
            "Hyperliquid submit outcome unresolved for {client_order_id}; do not resubmit automatically: {cause}"
        )))
    }
}

#[async_trait]
impl ExecutionAdapter for HyperliquidExecutionAdapter {
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        let client_order_id = intent.client_order_id();

        // Always reconcile the stable cloid first. Replaying a persisted intent after restart
        // therefore returns the existing venue order instead of issuing a second POST.
        if let Some(existing) = self.lookup_by_client_id(&client_order_id).await? {
            return Ok(VenueOrderAck {
                venue_order_id: existing.venue_order_id,
                client_order_id,
            });
        }

        let request = self.request_for_intent(intent).await?;
        match self.exchange.order(request, None).await {
            Ok(response) => match response_oid(&response) {
                Ok(Some(oid)) => Ok(VenueOrderAck {
                    venue_order_id: oid.to_string(),
                    client_order_id,
                }),
                Ok(None) => {
                    self.recover_submit(
                        &client_order_id,
                        "venue response contained no durable order id",
                    )
                    .await
                }
                Err(reason) => Err(ExecutionError::Rejected(reason)),
            },
            Err(error) if is_definite_pre_submit_error(&error) => Err(map_pre_submit_error(error)),
            Err(error) => self.recover_submit(&client_order_id, error.to_string()).await,
        }
    }

    async fn cancel(&self, order: OrderLocator<'_>) -> Result<(), ExecutionError> {
        let result = if let Some(venue_order_id) = order.venue_order_id {
            let oid = venue_order_id.parse::<u64>().map_err(|error| {
                ExecutionError::Conversion(format!("invalid Hyperliquid oid: {error}"))
            })?;
            self.exchange
                .cancel(
                    sdk::ClientCancelRequest {
                        asset: order.asset.to_string(),
                        oid,
                    },
                    None,
                )
                .await
        } else {
            let cloid = client_id_to_uuid(order.client_order_id)?;
            self.exchange
                .cancel_by_cloid(
                    sdk::ClientCancelRequestCloid {
                        asset: order.asset.to_string(),
                        cloid,
                    },
                    None,
                )
                .await
        };

        match result {
            Ok(sdk::ExchangeResponseStatus::Ok(_)) => Ok(()),
            Ok(sdk::ExchangeResponseStatus::Err(reason)) => Err(ExecutionError::Rejected(reason)),
            Err(error) if is_definite_pre_submit_error(&error) => Err(map_pre_submit_error(error)),
            Err(error) => Err(ExecutionError::Unknown(format!(
                "Hyperliquid cancel outcome is ambiguous; reconcile before retry: {error}"
            ))),
        }
    }

    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        let historical = self
            .info
            .historical_orders(self.account_address)
            .await
            .map_err(map_read_error)?;
        let by_oid = historical
            .into_iter()
            .map(|entry| (entry.order.oid, entry))
            .collect::<HashMap<_, _>>();
        self.info
            .open_orders(self.account_address)
            .await
            .map_err(map_read_error)?
            .into_iter()
            .map(|order| map_open_order(order, by_oid.get(&order.oid)))
            .collect()
    }

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        let state = self
            .info
            .user_state(self.account_address)
            .await
            .map_err(map_read_error)?;
        state
            .asset_positions
            .into_iter()
            .map(|position| {
                let quantity = Decimal::from_str(&position.position.szi)
                    .map_err(|error| ExecutionError::Conversion(error.to_string()))?;
                Ok(VenuePositionSnapshot {
                    asset: position.position.coin,
                    quantity,
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

fn base_url(network: HyperliquidNetwork) -> sdk::BaseUrl {
    match network {
        HyperliquidNetwork::Mainnet => sdk::BaseUrl::Mainnet,
        HyperliquidNetwork::Testnet => sdk::BaseUrl::Testnet,
    }
}

fn exact_size(quantity: Decimal, decimals: u32) -> Result<f64, ExecutionError> {
    let normalized = quantity.round_dp(decimals);
    if normalized != quantity {
        return Err(ExecutionError::Rejected(format!(
            "quantity {quantity} exceeds Hyperliquid size precision ({decimals} decimals)"
        )));
    }
    normalized.to_f64().filter(|value| value.is_finite() && *value > 0.0).ok_or_else(|| {
        ExecutionError::Conversion("quantity cannot be represented as a positive f64".into())
    })
}

fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn round_to_significant_and_decimal(value: f64, sig_figs: u32, max_decimals: u32) -> f64 {
    let abs_value = value.abs();
    if abs_value == 0.0 {
        return 0.0;
    }
    let magnitude = abs_value.log10().floor() as i32;
    let scale = 10f64.powi(sig_figs as i32 - magnitude - 1);
    let rounded = (abs_value * scale).round() / scale;
    round_to_decimals(rounded.copysign(value), max_decimals)
}

fn venue_cloid(intent_id: Uuid) -> String {
    format!("0x{}", intent_id.simple())
}

fn client_id_to_uuid(client_order_id: &str) -> Result<Uuid, ExecutionError> {
    let hex = client_order_id.strip_prefix("pg").ok_or_else(|| {
        ExecutionError::Conversion("Hyperliquid client id must start with pg".into())
    })?;
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ExecutionError::Conversion(
            "Hyperliquid client id must contain exactly 32 hex digits".into(),
        ));
    }
    Uuid::parse_str(hex).map_err(|error| ExecutionError::Conversion(error.to_string()))
}

fn client_id_to_venue_cloid(client_order_id: &str) -> Result<String, ExecutionError> {
    Ok(venue_cloid(client_id_to_uuid(client_order_id)?))
}

fn venue_cloid_to_client_id(cloid: &str) -> Option<String> {
    let hex = cloid.strip_prefix("0x").unwrap_or(cloid);
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("pg{}", hex.to_ascii_lowercase()))
}

fn map_open_order(
    order: sdk::OpenOrdersResponse,
    historical: Option<&sdk::OrderInfo>,
) -> Result<VenueOrderSnapshot, ExecutionError> {
    if let Some(historical) = historical {
        return map_historical_order(historical.clone());
    }
    let remaining = Decimal::from_str(&order.sz)
        .map_err(|error| ExecutionError::Conversion(error.to_string()))?;
    Ok(VenueOrderSnapshot {
        venue_order_id: order.oid.to_string(),
        client_order_id: order.cloid.as_deref().and_then(venue_cloid_to_client_id),
        asset: order.coin,
        side: map_side(&order.side)?,
        requested_quantity: remaining,
        filled_quantity: Decimal::ZERO,
        limit_price: Some(
            Decimal::from_str(&order.limit_px)
                .map_err(|error| ExecutionError::Conversion(error.to_string()))?,
        ),
        state: VenueOrderState::Open,
    })
}

fn map_historical_order(order: sdk::OrderInfo) -> Result<VenueOrderSnapshot, ExecutionError> {
    let requested = Decimal::from_str(&order.order.orig_sz)
        .map_err(|error| ExecutionError::Conversion(error.to_string()))?;
    let remaining = Decimal::from_str(&order.order.sz)
        .map_err(|error| ExecutionError::Conversion(error.to_string()))?;
    let state = map_state(&order.status, requested, remaining);
    let filled = if state == VenueOrderState::Filled {
        requested
    } else {
        (requested - remaining).max(Decimal::ZERO)
    };
    Ok(VenueOrderSnapshot {
        venue_order_id: order.order.oid.to_string(),
        client_order_id: order
            .order
            .cloid
            .as_deref()
            .and_then(venue_cloid_to_client_id),
        asset: order.order.coin,
        side: map_side(&order.order.side)?,
        requested_quantity: requested,
        filled_quantity: filled,
        limit_price: Some(
            Decimal::from_str(&order.order.limit_px)
                .map_err(|error| ExecutionError::Conversion(error.to_string()))?,
        ),
        state,
    })
}

fn map_side(side: &str) -> Result<Side, ExecutionError> {
    match side.to_ascii_uppercase().as_str() {
        "B" | "BUY" => Ok(Side::Buy),
        "A" | "S" | "SELL" => Ok(Side::Sell),
        other => Err(ExecutionError::Conversion(format!(
            "unknown Hyperliquid side {other}"
        ))),
    }
}

fn map_state(status: &str, requested: Decimal, remaining: Decimal) -> VenueOrderState {
    let status = status.to_ascii_lowercase();
    if status == "filled" {
        VenueOrderState::Filled
    } else if status.contains("cancel") {
        VenueOrderState::Canceled
    } else if status.contains("reject") {
        VenueOrderState::Rejected
    } else if status == "open" {
        if remaining < requested {
            VenueOrderState::PartiallyFilled
        } else {
            VenueOrderState::Open
        }
    } else {
        VenueOrderState::Unknown
    }
}

fn response_oid(response: &sdk::ExchangeResponseStatus) -> Result<Option<u64>, String> {
    match response {
        sdk::ExchangeResponseStatus::Err(reason) => Err(reason.clone()),
        sdk::ExchangeResponseStatus::Ok(response) => {
            let Some(data) = response.data.as_ref() else {
                return Ok(None);
            };
            match data.statuses.first() {
                Some(sdk::ExchangeDataStatus::Resting(order)) => Ok(Some(order.oid)),
                Some(sdk::ExchangeDataStatus::Filled(order)) => Ok(Some(order.oid)),
                Some(sdk::ExchangeDataStatus::Error(reason)) => Err(reason.clone()),
                _ => Ok(None),
            }
        }
    }
}

fn is_definite_pre_submit_error(error: &sdk::Error) -> bool {
    matches!(
        error,
        sdk::Error::AssetNotFound
            | sdk::Error::Wallet(_)
            | sdk::Error::PrivateKeyParse(_)
            | sdk::Error::SignatureFailure(_)
            | sdk::Error::FloatStringParse
            | sdk::Error::OrderTypeNotFound
            | sdk::Error::ChainNotAllowed
            | sdk::Error::NoCloid
    )
}

fn map_pre_submit_error(error: sdk::Error) -> ExecutionError {
    match error {
        sdk::Error::Wallet(message)
        | sdk::Error::PrivateKeyParse(message)
        | sdk::Error::SignatureFailure(message) => ExecutionError::Authentication(message),
        other => ExecutionError::Conversion(other.to_string()),
    }
}

fn map_connect_error(error: sdk::Error) -> ExecutionError {
    match error {
        sdk::Error::Wallet(message)
        | sdk::Error::PrivateKeyParse(message)
        | sdk::Error::SignatureFailure(message) => ExecutionError::Authentication(message),
        sdk::Error::ClientRequest { .. }
        | sdk::Error::ServerRequest { .. }
        | sdk::Error::GenericRequest(_)
        | sdk::Error::Websocket(_)
        | sdk::Error::WsSend(_) => ExecutionError::Transport(error.to_string()),
        other => ExecutionError::Conversion(other.to_string()),
    }
}

fn map_read_error(error: sdk::Error) -> ExecutionError {
    match error {
        sdk::Error::ClientRequest { .. }
        | sdk::Error::ServerRequest { .. }
        | sdk::Error::GenericRequest(_)
        | sdk::Error::Websocket(_)
        | sdk::Error::WsSend(_) => ExecutionError::Transport(error.to_string()),
        other => ExecutionError::Conversion(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloid_round_trip_uses_persisted_intent_uuid() {
        let id = Uuid::parse_str("018f7f2e-6f5c-7cc4-98e8-2cd9b5c67d0f").unwrap();
        let client_id = format!("pg{}", id.simple());
        assert_eq!(
            client_id_to_venue_cloid(&client_id).unwrap(),
            "0x018f7f2e6f5c7cc498e82cd9b5c67d0f"
        );
        assert_eq!(
            venue_cloid_to_client_id("0x018F7F2E6F5C7CC498E82CD9B5C67D0F").unwrap(),
            client_id
        );
    }

    #[test]
    fn canceled_status_wins_over_embedded_filled_word() {
        assert_eq!(
            map_state(
                "siblingFilledCanceled",
                Decimal::from(10),
                Decimal::from(5)
            ),
            VenueOrderState::Canceled
        );
    }

    #[test]
    fn open_partial_fill_is_detected_from_remaining_size() {
        assert_eq!(
            map_state("open", Decimal::from(10), Decimal::from(4)),
            VenueOrderState::PartiallyFilled
        );
    }

    #[test]
    fn size_precision_is_fail_closed() {
        let error = exact_size(Decimal::new(12345, 4), 3).unwrap_err();
        assert!(matches!(error, ExecutionError::Rejected(_)));
    }
}
