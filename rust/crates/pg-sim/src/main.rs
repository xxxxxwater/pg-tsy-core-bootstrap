//! Credential-free deterministic one-request-per-simulation JSONL bridge.
//! No venue SDK, network socket, exchange credentials or real-order authority.
use pg_sim::{MarketPhase, MarketSnapshot, MatchOutcome, MatchingEngine, SimOrder};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

const MAX_LINE_BYTES: usize = 1_048_576;

#[derive(Deserialize)]
struct WireTop {
    bid_price: Decimal,
    bid_quantity: Decimal,
    ask_price: Decimal,
    ask_quantity: Decimal,
}

#[derive(Deserialize)]
struct WireRequest {
    request_id: u64,
    order: Value,
    top: WireTop,
    #[serde(default)]
    position: Decimal,
    #[serde(default)]
    now_ns: u64,
    #[serde(default = "default_session")]
    session: String,
}

fn default_session() -> String {
    "CONTINUOUS".to_owned()
}

fn parse_phase(value: &str) -> Result<MarketPhase, &'static str> {
    match value {
        "PRE_OPEN" => Ok(MarketPhase::PreOpen),
        "OPENING" => Ok(MarketPhase::Opening),
        "CONTINUOUS" => Ok(MarketPhase::Continuous),
        "CLOSING" => Ok(MarketPhase::Closing),
        "CLOSED" => Ok(MarketPhase::Closed),
        _ => Err("invalid session: expected PRE_OPEN, OPENING, CONTINUOUS, CLOSING or CLOSED"),
    }
}

fn simulate(request: WireRequest) -> Result<Value, String> {
    // Accept direct SimOrder and {"base": SimOrder} research envelopes.
    let base = request.order.get("base").unwrap_or(&request.order);
    let order: SimOrder = serde_json::from_value(base.clone())
        .map_err(|error| format!("invalid order: {error}"))?;
    let phase = parse_phase(&request.session).map_err(str::to_owned)?;
    let top = request.top;
    if top.bid_price <= Decimal::ZERO
        || top.ask_price <= Decimal::ZERO
        || top.bid_price > top.ask_price
        || top.bid_quantity < Decimal::ZERO
        || top.ask_quantity < Decimal::ZERO
    {
        return Err("invalid top of book: require positive noncrossed prices and nonnegative sizes".into());
    }
    let market = MarketSnapshot {
        bid_price: top.bid_price,
        bid_quantity: top.bid_quantity,
        ask_price: top.ask_price,
        ask_quantity: top.ask_quantity,
        phase,
        now_ns: request.now_ns,
    };
    // Persistent process for batching; isolated matching state per counterfactual.
    let mut engine = MatchingEngine::default();
    engine.submit(order.clone(), true).map_err(|error| error.to_string())?;
    let outcome = engine
        .match_once(order.order_id, market, request.position)
        .map_err(|error| error.to_string())?;
    let record = engine
        .record(order.order_id)
        .ok_or_else(|| "simulation order record unavailable".to_owned())?;
    let (name, fill, reason) = match outcome {
        MatchOutcome::Filled(fill) => (
            "Filled",
            Some(json!({"quantity": fill.quantity.to_string(), "price": fill.price.to_string()})),
            None,
        ),
        MatchOutcome::PartiallyFilled(fill) => (
            "PartiallyFilled",
            Some(json!({"quantity": fill.quantity.to_string(), "price": fill.price.to_string()})),
            None,
        ),
        MatchOutcome::Resting => ("Resting", None, None),
        MatchOutcome::Canceled => ("Canceled", None, None),
        MatchOutcome::Rejected(reason) => ("Rejected", None, Some(reason)),
        MatchOutcome::Dormant => ("Dormant", None, None),
    };
    Ok(json!({
        "ok": true,
        "outcome": name,
        "state": format!("{:?}", record.state),
        "order_id": order.order_id,
        "filled_quantity": record.filled_quantity.to_string(),
        "remaining_quantity": record.remaining().to_string(),
        "fill": fill,
        "reason": reason,
    }))
}

fn handle_line(line: &str) -> Value {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return json!({"request_id": null, "result": {"ok": false, "error": format!("invalid JSON: {error}")}});
        }
    };
    let request_id = parsed.get("request_id").cloned().unwrap_or(Value::Null);
    let result = match serde_json::from_value::<WireRequest>(parsed) {
        Ok(request) => match simulate(request) {
            Ok(result) => result,
            Err(error) => json!({"ok": false, "error": error}),
        },
        Err(error) => json!({"ok": false, "error": format!("invalid request: {error}")}),
    };
    json!({"request_id": request_id, "result": result})
}

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line?;
        let response = if line.len() > MAX_LINE_BYTES {
            json!({"request_id": null, "result": {"ok": false, "error": "request exceeds 1 MiB limit"}})
        } else {
            handle_line(&line)
        };
        serde_json::to_writer(&mut stdout, &response).map_err(io::Error::other)?;
        stdout.write_all(b"\n")?;
        // Flush each record: Python writes batches and reads responses in order.
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> String {
        json!({
            "request_id": 7,
            "order": {
                "order_id": "f4c8c02e-5e3a-4076-8714-69137e7dcd2e",
                "side": "Buy", "kind": "Limit", "quantity": "2",
                "limit_price": "101", "time_in_force": "Ioc",
                "expire_at_ns": null, "post_only": false,
                "reduce_only": false, "display_quantity": null,
                "contingency": null
            },
            "top": {
                "bid_price": "99", "bid_quantity": "4",
                "ask_price": "100", "ask_quantity": "3"
            },
            "position": "0", "now_ns": 42, "session": "CONTINUOUS"
        })
        .to_string()
    }

    #[test]
    fn direct_order_fills_and_preserves_request_identity() {
        let response = handle_line(&request());
        assert_eq!(response["request_id"], 7);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["outcome"], "Filled");
        assert_eq!(response["result"]["filled_quantity"], "2");
        assert_eq!(response["result"]["fill"]["price"], "100");
    }

    #[test]
    fn malformed_input_fails_without_inventing_a_fill() {
        let response = handle_line("not-json");
        assert_eq!(response["result"]["ok"], false);
        assert!(response["result"]["fill"].is_null());
    }

    #[test]
    fn independent_requests_do_not_share_simulated_position() {
        let input = request();
        assert_eq!(handle_line(&input)["result"], handle_line(&input)["result"]);
    }

    #[test]
    fn invalid_book_is_rejected() {
        let mut input: Value = serde_json::from_str(&request()).unwrap();
        input["top"]["bid_price"] = json!("102");
        assert_eq!(handle_line(&input.to_string())["result"]["ok"], false);
    }
}
