//! Real Binance Portfolio Margin REST boundary. No implicit retry, no logging of
//! credentials/signatures, and no exposure-increasing call without an explicit gate.
//! The caller MUST journal and fence before invoking `submit_order`.

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
        TRADES_PATH, lookup_by_client_id, normalize_order,
    },
};

type HmacSha256 = Hmac<Sha256>;
const PM_BASE: &str = "https://papi.binance.com";
const MAX_BODY: usize = 1024 * 1024;
const RECV_WINDOW_MS: u64 = 5_000;

/// All credentials must come from the operator's secret store, never fixtures or
/// repository configuration. `enable_submission` is FALSE unless explicitly set.
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

    /// Canonical form encoding is signed verbatim; do not re-encode between HMAC
    /// generation and transmission. Timestamp is generated at dispatch time.
    pub fn signed_form(
        &self,
        params: &[(String, String)],
        timestamp_ms: u64,
    ) -> Result<String, ExecutionError> {
        if timestamp_ms == 0 || params.iter().any(|(key, _)| key == "signature" || key == "timestamp") {
            return Err(ExecutionError::Conversion("invalid signed request parameters".into()));
        }
        let mut url = reqwest::Url::parse(PM_BASE)
            .map_err(|_| ExecutionError::Conversion("invalid fixed PM endpoint".into()))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                pairs.append_pair(key, value);
            }
            pairs.append_pair("recvWindow", &RECV_WINDOW_MS.to_string());
            pairs.append_pair("timestamp", &timestamp_ms.to_string());
        }
        let unsigned = url.query().ok_or_else(|| ExecutionError::Conversion("missing query".into()))?;
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .map_err(|_| ExecutionError::Authentication("cannot initialize signer".into()))?;
        mac.update(unsigned.as_bytes());
        let signature = mac.finalize().into_bytes();
        let mut out = String::with_capacity(unsigned.len() + 75);
        out.push_str(unsigned);
        out.push_str("&signature=");
        for byte in signature {
            use std::fmt::Write;
            write!(&mut out, "{byte:02x}")
                .map_err(|_| ExecutionError::Conversion("cannot encode signature".into()))?;
        }
        Ok(out)
    }

    async fn signed(
        &self,
        method: Method,
        path: &'static str,
        params: &[(String, String)],
    ) -> Result<Value, ExecutionError> {
        // No caller-controlled URL or host and no automatic mutating retries.
        if !matches!(path, NEW_ORDER_PATH | QUERY_ORDER_PATH | ALL_ORDERS_PATH | TRADES_PATH
            | "/papi/v1/um/openOrders" | "/papi/v1/um/positionRisk")
        {
            return Err(ExecutionError::Unsupported("unapproved PM endpoint".into()));
        }
        if method != Method::GET && !self.enable_submission {
            return Err(ExecutionError::Unsupported("Binance trade capability disabled".into()));
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ExecutionError::Unknown("system clock invalid".into()))?
            .as_millis();
        let now_ms = u64::try_from(now_ms)
            .map_err(|_| ExecutionError::Unknown("system clock overflow".into()))?;
        let form = self.signed_form(params, now_ms)?;
        let endpoint = format!("{PM_BASE}{path}");
        let request = self.client.request(method.clone(), &endpoint)
            .header("X-MBX-APIKEY", &self.api_key);
        let request = if method == Method::POST {
            request.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(form)
        } else {
            request.query(&[]) // do not let the client transform the signed bytes
                .build()
                .map_err(|_| ExecutionError::Conversion("cannot build request".into()))?;
            self.client.request(method.clone(), format!("{endpoint}?{form}"))
                .header("X-MBX-APIKEY", &self.api_key)
        };
        let response = request.send().await.map_err(|_| {
            if method == Method::GET {
                ExecutionError::Transport("PM read transport failed".into())
            } else {
                ExecutionError::Unknown("PM mutation outcome unknown; query by client id".into())
            }
        })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|_| {
            if method == Method::GET {
                ExecutionError::Transport("PM response incomplete".into())
            } else {
                ExecutionError::Unknown("PM mutation response incomplete".into())
            }
        })?;
        if body.len() > MAX_BODY {
            return Err(ExecutionError::Unknown("oversized PM response; reconcile".into()));
        }
        if status != StatusCode::OK {
            // Even an HTTP error after POST is not proof that the venue did not
            // create the order. Never classify it as a safe rejection here.
            return Err(if method == Method::GET {
                ExecutionError::Transport(format!("PM read HTTP {status}"))
            } else {
                ExecutionError::Unknown(format!("PM mutation HTTP {status}; reconcile"))
            });
        }
        serde_json::from_slice(&body).map_err(|_| {
            ExecutionError::Unknown("unparseable PM response; reconcile".into())
        })
    }

    pub async fn submit_order(&self, prepared: &PreparedOrder) -> Result<VenueOrderAck, ExecutionError> {
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
        let venue_order_id = response.get("orderId").and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .ok_or_else(|| ExecutionError::Unknown("submit response lacks order id".into()))?;
        let durable_id = durable_client_order_id(client_id)
            .ok_or_else(|| ExecutionError::Unknown("submit id is not recoverable".into()))?;
        Ok(VenueOrderAck { venue_order_id: venue_order_id.to_string(), client_order_id: durable_id })
    }

    /// Query current or terminal state using a stable venue ID, not a new order.
    /// A missing result must remain unresolved until history and fills are read.
    pub async fn query_order(&self, venue_client_id: &str) -> Result<VenueOrderSnapshot, ExecutionError> {
        let params = lookup_by_client_id(venue_client_id)
            .map_err(|_| ExecutionError::Conversion("unowned client id".into()))?
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<Vec<_>>();
        let json = self.signed(Method::GET, QUERY_ORDER_PATH, &params).await?;
        decode_pm_order(&json, venue_client_id)
    }

    /// Read-only venue truth. Reject unknown/manual IDs for this strict adapter:
    /// silently filtering them would allow the orchestrator to claim clean reconciliation.
    pub async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        let params = vec![("symbol".to_owned(), SYMBOL.to_owned())];
        let json = self.signed(Method::GET, "/papi/v1/um/openOrders", &params).await?;
        let orders = json.as_array().ok_or_else(|| ExecutionError::Conversion("invalid open orders".into()))?;
        orders.iter().map(|order| {
            let client_id = order.get("clientOrderId").and_then(Value::as_str)
                .ok_or_else(|| ExecutionError::Conversion("missing order ownership".into()))?;
            decode_pm_order(order, client_id)
        }).collect()
    }

    /// The caller must paginate and cross-check `userTrades` before declaring a
    /// missing/canceled/expired order terminal after a lost acknowledgement.
    pub async fn recent_orders(&self) -> Result<Value, ExecutionError> {
        self.signed(Method::GET, ALL_ORDERS_PATH, &[("symbol".into(), SYMBOL.into()), ("limit".into(), "1000".into())]).await
    }

    pub async fn recent_trades(&self) -> Result<Value, ExecutionError> {
        self.signed(Method::GET, TRADES_PATH, &[("symbol".into(), SYMBOL.into()), ("limit".into(), "1000".into())]).await
    }

    pub async fn position_risk(&self) -> Result<Value, ExecutionError> {
        self.signed(Method::GET, "/papi/v1/um/positionRisk", &[("symbol".into(), SYMBOL.into())]).await
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, ExecutionError> {
    value.get(key).and_then(Value::as_str)
        .ok_or_else(|| ExecutionError::Conversion("malformed PM order response".into()))
}

/// REST replies and WS events must expose the SAME durable ID to the OMS.
pub fn decode_pm_order(value: &Value, expected_venue_id: &str) -> Result<VenueOrderSnapshot, ExecutionError> {
    let order_id = value.get("orderId").and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| ExecutionError::Conversion("missing PM order id".into()))?.to_string();
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
    fn signature_has_stable_encoding_and_never_includes_secret() {
        let client = BinanceRestClient::new("key".into(), "secret".into(), false).unwrap();
        let request = client.signed_form(&[("symbol".into(), "BTCUSDC".into()), ("quantity".into(), "0.01".into())], 1_700_000_000_000).unwrap();
        assert!(request.starts_with("symbol=BTCUSDC&quantity=0.01&recvWindow=5000&timestamp=1700000000000&signature="));
        assert_eq!(request.split("&signature=").nth(1).unwrap().len(), 64);
        assert!(!request.contains("secret"));
        assert!(client.signed_form(&[("signature".into(), "forged".into())], 1).is_err());
    }

    #[test]
    fn rest_order_matches_durable_oms_identity_and_rejects_manual() {
        let id = encode_intent_bytes(&[0x42; 16]);
        let value = serde_json::json!({"symbol":"BTCUSDC", "clientOrderId":id, "orderId":123,
            "side":"BUY", "origQty":"0.010", "executedQty":"0.005", "price":"80000", "status":"PARTIALLY_FILLED"});
        let order = decode_pm_order(&value, &id).unwrap();
        assert_eq!(order.client_order_id.unwrap(), format!("pg{}", "42".repeat(16)));
        assert!(decode_pm_order(&value, "manual-order").is_err());
    }

    #[tokio::test]
    async fn submit_disabled_by_default_cannot_reach_network() {
        let client = BinanceRestClient::new("key".into(), "secret".into(), false).unwrap();
        let prepared = PreparedOrder { path: NEW_ORDER_PATH, params: vec![("symbol", SYMBOL.into())], client_id: encode_intent_bytes(&[0x42; 16]) };
        assert!(matches!(client.submit_order(&prepared).await, Err(ExecutionError::Unsupported(_))));
    }
}
