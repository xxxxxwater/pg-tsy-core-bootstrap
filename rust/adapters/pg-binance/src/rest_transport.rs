//! Strict Portfolio Margin REST transport. Not registered in the production
//! daemon. Never retry a mutation or log credentials, signatures or listen keys.
//! The caller must persist intent and verify its fencing lease before mutation.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use pg_execution::{ExecutionError, VenueOrderAck, VenueOrderSnapshot};
use reqwest::{Client, Method, StatusCode, header};
use serde_json::Value;
use sha2::Sha256;

use crate::{
    durable_client_order_id,
    order_protocol::{
        ALL_ORDERS_PATH, NEW_ORDER_PATH, PreparedOrder, QUERY_ORDER_PATH, RawOrder, SYMBOL,
        TRADES_PATH, lookup_by_client_id, normalize_order, valid_client_order_id,
    },
};

type HmacSha256 = Hmac<Sha256>;
const PM_BASE: &str = "https://papi.binance.com";
const MAX_BODY: usize = 1024 * 1024;
const RECV_WINDOW_MS: u64 = 5_000;

/// Credentials must originate in an operator-controlled secret store. The
/// capability to send any mutating request defaults to disabled.
pub struct BinanceRestClient {
    client: Client,
    api_key: String,
    secret: String,
    enable_submission: bool,
}

impl BinanceRestClient {
    pub fn new(
        api_key: String,
        secret: String,
        enable_submission: bool,
    ) -> Result<Self, ExecutionError> {
        if api_key.is_empty() || secret.is_empty() || api_key.len() > 512 || secret.len() > 512 {
            return Err(ExecutionError::Authentication("invalid Binance credentials".into()));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ExecutionError::Transport("cannot initialize TLS HTTP client".into()))?;
        Ok(Self { client, api_key, secret, enable_submission })
    }

    /// Sign the exact bytes to transmit. No caller-supplied timestamp or
    /// signature, no secondary encoding or automatic transport retry.
    pub fn signed_form(
        &self,
        params: &[(String, String)],
        timestamp_ms: u64,
    ) -> Result<String, ExecutionError> {
        if timestamp_ms == 0
            || params.iter().any(|(key, _)| key == "signature" || key == "timestamp")
        {
            return Err(ExecutionError::Conversion("invalid signed parameters".into()));
        }
        let mut url = reqwest::Url::parse(PM_BASE)
            .map_err(|_| ExecutionError::Conversion("invalid PM endpoint".into()))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                pairs.append_pair(key, value);
            }
            pairs.append_pair("recvWindow", &RECV_WINDOW_MS.to_string());
            pairs.append_pair("timestamp", &timestamp_ms.to_string());
        }
        let unsigned = url.query()
            .ok_or_else(|| ExecutionError::Conversion("missing signed query".into()))?;
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .map_err(|_| ExecutionError::Authentication("cannot initialize signer".into()))?;
        mac.update(unsigned.as_bytes());
        let mut form = String::with_capacity(unsigned.len() + 75);
        form.push_str(unsigned);
        form.push_str("&signature=");
        for byte in mac.finalize().into_bytes() {
            use std::fmt::Write;
            write!(&mut form, "{byte:02x}")
                .map_err(|_| ExecutionError::Conversion("cannot encode signature".into()))?;
        }
        Ok(form)
    }

    async fn signed(
        &self,
        method: Method,
        path: &'static str,
        params: &[(String, String)],
    ) -> Result<Value, ExecutionError> {
        if !matches!(
            path,
            NEW_ORDER_PATH
                | ALL_ORDERS_PATH
                | TRADES_PATH
                | "/papi/v1/um/openOrders"
                | "/papi/v1/um/positionRisk"
        ) {
            return Err(ExecutionError::Unsupported("unapproved PM endpoint".into()));
        }
        if method != Method::GET && method != Method::POST && method != Method::DELETE {
            return Err(ExecutionError::Unsupported("unapproved PM method".into()));
        }
        if method != Method::GET && path != NEW_ORDER_PATH {
            return Err(ExecutionError::Unsupported("unapproved PM mutation".into()));
        }
        if method != Method::GET && !self.enable_submission {
            return Err(ExecutionError::Unsupported("Binance mutation capability disabled".into()));
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ExecutionError::Unknown("system clock invalid".into()))?
            .as_millis();
        let now_ms = u64::try_from(now_ms)
            .map_err(|_| ExecutionError::Unknown("system clock overflow".into()))?;
        let form = self.signed_form(params, now_ms)?;
        let endpoint = format!("{PM_BASE}{path}");
        let request = if method == Method::POST {
            self.client.post(endpoint)
                .header("X-MBX-APIKEY", &self.api_key)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(form)
        } else {
            // Binance signed GET/DELETE signatures use the exact query string.
            self.client.request(method.clone(), format!("{endpoint}?{form}"))
                .header("X-MBX-APIKEY", &self.api_key)
        };
        let response = request.send().await.map_err(|_| {
            if method == Method::GET {
                ExecutionError::Transport("PM read transport failed".into())
            } else {
                ExecutionError::Unknown("PM mutation outcome unknown; reconcile".into())
            }
        })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|_| {
            if method == Method::GET {
                ExecutionError::Transport("PM response incomplete".into())
            } else {
                ExecutionError::Unknown("PM mutation response incomplete; reconcile".into())
            }
        })?;
        if body.len() > MAX_BODY {
            return Err(ExecutionError::Unknown("oversized PM response; reconcile".into()));
        }
        if status != StatusCode::OK {
            return Err(if method == Method::GET {
                ExecutionError::Transport(format!("PM read HTTP {status}"))
            } else {
                ExecutionError::Unknown(format!("PM mutation HTTP {status}; reconcile"))
            });
        }
        serde_json::from_slice(&body)
            .map_err(|_| ExecutionError::Unknown("unparseable PM response; reconcile".into()))
    }

    /// Call only after pg-orchestrator journals intent and checks fencing.
    pub async fn submit_order(
        &self,
        prepared: &PreparedOrder,
    ) -> Result<VenueOrderAck, ExecutionError> {
        if prepared.path != NEW_ORDER_PATH || prepared.get("symbol") != Some(SYMBOL) {
            return Err(ExecutionError::Conversion("unexpected prepared order".into()));
        }
        let params = prepared.params.iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect::<Vec<_>>();
        let response = self.signed(Method::POST, NEW_ORDER_PATH, &params).await?;
        let client_id = response.get("clientOrderId").and_then(Value::as_str)
            .ok_or_else(|| ExecutionError::Unknown("submit response lacks client id".into()))?;
        if client_id != prepared.client_id {
            return Err(ExecutionError::Unknown("submit client id mismatch".into()));
        }
        let order_id = response.get("orderId").and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .ok_or_else(|| ExecutionError::Unknown("submit response lacks order id".into()))?;
        let durable_id = durable_client_order_id(client_id)
            .ok_or_else(|| ExecutionError::Unknown("submit id is not recoverable".into()))?;
        Ok(VenueOrderAck {
            venue_order_id: order_id.to_string(),
            client_order_id: durable_id,
        })
    }

    /// A successful DELETE acknowledgment is NOT proof of zero fills. The
    /// durable caller must immediately query order history and reconcile it.
    /// A timeout or a 4xx/5xx after dispatch is UNKNOWN, never permission to
    /// resubmit the original order with a new ID.
    pub async fn cancel_order(&self, venue_client_id: &str) -> Result<(), ExecutionError> {
        if !valid_client_order_id(venue_client_id) {
            return Err(ExecutionError::Conversion("unowned cancel client id".into()));
        }
        let params = lookup_by_client_id(venue_client_id)
            .map_err(|_| ExecutionError::Conversion("unowned cancel client id".into()))?
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<Vec<_>>();
        let response = self.signed(Method::DELETE, NEW_ORDER_PATH, &params).await?;
        if response.get("symbol").and_then(Value::as_str) != Some(SYMBOL)
            || response.get("clientOrderId").and_then(Value::as_str) != Some(venue_client_id)
            || !matches!(response.get("status").and_then(Value::as_str), Some("CANCELED" | "FILLED" | "EXPIRED"))
        {
            return Err(ExecutionError::Unknown("cancel acknowledgment cannot prove ownership or final status".into()));
        }
        Ok(())
    }

    /// Reads both open and terminal order truth. A missing query remains an
    /// error: a 404 does not prove the order was never received.
    pub async fn query_order(
        &self,
        venue_client_id: &str,
    ) -> Result<VenueOrderSnapshot, ExecutionError> {
        let params = lookup_by_client_id(venue_client_id)
            .map_err(|_| ExecutionError::Conversion("unowned client id".into()))?
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<Vec<_>>();
        let json = self.signed(Method::GET, QUERY_ORDER_PATH, &params).await?;
        decode_pm_order(&json, venue_client_id)
    }

    /// Reject foreign/manual orders rather than silently losing mismatch data.
    pub async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        let json = self.signed(Method::GET, "/papi/v1/um/openOrders",
            &[("symbol".into(), SYMBOL.into())]).await?;
        let orders = json.as_array()
            .ok_or_else(|| ExecutionError::Conversion("invalid open orders".into()))?;
        orders.iter().map(|order| {
            let client_id = order.get("clientOrderId").and_then(Value::as_str)
                .ok_or_else(|| ExecutionError::Conversion("missing order ownership".into()))?;
            decode_pm_order(order, client_id)
        }).collect()
    }

    /// Raw bounded pages only; NOT proof that all history/fills were recovered.
    pub async fn recent_orders(&self) -> Result<Value, ExecutionError> {
        self.signed(Method::GET, ALL_ORDERS_PATH,
            &[("symbol".into(), SYMBOL.into()), ("limit".into(), "1000".into())]).await
    }

    pub async fn recent_trades(&self) -> Result<Value, ExecutionError> {
        self.signed(Method::GET, TRADES_PATH,
            &[("symbol".into(), SYMBOL.into()), ("limit".into(), "1000".into())]).await
    }

    pub async fn position_risk(&self) -> Result<Value, ExecutionError> {
        self.signed(Method::GET, "/papi/v1/um/positionRisk",
            &[("symbol".into(), SYMBOL.into())]).await
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, ExecutionError> {
    value.get(key).and_then(Value::as_str)
        .ok_or_else(|| ExecutionError::Conversion("malformed PM order response".into()))
}

/// Both REST and user-stream replies expose the 34-character durable OMS ID.
pub fn decode_pm_order(
    value: &Value,
    expected_venue_id: &str,
) -> Result<VenueOrderSnapshot, ExecutionError> {
    let order_id = value.get("orderId").and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| ExecutionError::Conversion("missing PM order id".into()))?
        .to_string();
    let mut snapshot = normalize_order(RawOrder {
        symbol: field(value, "symbol")?,
        client_order_id: field(value, "clientOrderId")?,
        order_id: &order_id,
        side: field(value, "side")?,
        original_quantity: field(value, "origQty")?,
        executed_quantity: field(value, "executedQty")?,
        price: field(value, "price")?,
        status: field(value, "status")?,
    }, expected_venue_id).map_err(|_| ExecutionError::Conversion("invalid PM order truth".into()))?;
    snapshot.client_order_id = durable_client_order_id(expected_venue_id);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_intent_bytes;

    #[test]
    fn signature_is_deterministic_and_never_contains_secret() {
        let client = BinanceRestClient::new("key".into(), "secret".into(), false).unwrap();
        let form = client.signed_form(
            &[("symbol".into(), "BTCUSDC".into()), ("quantity".into(), "0.01".into())],
            1_700_000_000_000,
        ).unwrap();
        assert!(form.starts_with("symbol=BTCUSDC&quantity=0.01&recvWindow=5000&timestamp=1700000000000&signature="));
        assert_eq!(form.split("&signature=").nth(1).unwrap().len(), 64);
        assert!(!form.contains("secret"));
        assert!(client.signed_form(&[("signature".into(), "forged".into())], 1).is_err());
    }

    #[test]
    fn rest_order_maps_to_durable_identity_and_rejects_manual() {
        let id = encode_intent_bytes(&[0x42; 16]);
        let value = serde_json::json!({
            "symbol":"BTCUSDC", "clientOrderId":id, "orderId":123,
            "side":"BUY", "origQty":"0.010", "executedQty":"0.005",
            "price":"80000", "status":"PARTIALLY_FILLED"
        });
        let order = decode_pm_order(&value, &id).unwrap();
        assert_eq!(order.client_order_id.unwrap(), format!("pg{}", "42".repeat(16)));
        assert!(decode_pm_order(&value, "manual-order").is_err());
    }

    #[tokio::test]
    async fn disabled_mutations_fail_before_network() {
        let client = BinanceRestClient::new("key".into(), "secret".into(), false).unwrap();
        let id = encode_intent_bytes(&[0x42; 16]);
        let prepared = PreparedOrder {
            path: NEW_ORDER_PATH,
            params: vec![("symbol", SYMBOL.into())],
            client_id: id.clone(),
        };
        assert!(matches!(client.submit_order(&prepared).await, Err(ExecutionError::Unsupported(_))));
        assert!(matches!(client.cancel_order(&id).await, Err(ExecutionError::Unsupported(_))));
        assert!(matches!(client.cancel_order("foreign").await, Err(ExecutionError::Conversion(_))));
    }
}
