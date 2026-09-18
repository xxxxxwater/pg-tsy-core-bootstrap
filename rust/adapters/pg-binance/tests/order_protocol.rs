//! Offline fixtures for the venue wire contract. No API key and no network.

use std::str::FromStr;

use pg_binance::order_protocol::{
    OrderStyle, PositionMode, ProtocolError, SymbolFilters, prepare_order,
};
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::Decimal;

fn intent() -> OrderIntent {
    OrderIntent {
        intent_id: "018f7f2e-6f5c-7cc4-98e8-2cd9b5c67d0f".parse().unwrap(),
        strategy_id: "jev-btcusdc-shadow".to_owned(),
        asset: "BTCUSDC".to_owned(),
        venue: Venue::BinancePm,
        side: Side::Buy,
        quantity: Decimal::from_str("0.010").unwrap(),
        limit_price: Some(Decimal::from_str("80000.0").unwrap()),
        effect: ExposureEffect::Increase,
        source_signal_id: Some("fixture-1".to_owned()),
    }
}

fn filters() -> SymbolFilters {
    // Test-only fixtures; NEVER substitute these values for exchangeInfo.
    SymbolFilters {
        symbol: "BTCUSDC".to_owned(),
        tick_size: "0.10".to_owned(),
        step_size: "0.001".to_owned(),
        min_quantity: "0.001".to_owned(),
        min_notional: "10".to_owned(),
        symbol_trading: true,
    }
}

#[test]
fn post_only_wire_fields_are_deterministic_and_btcusdc_only() {
    let a = prepare_order(&intent(), &filters(), PositionMode::OneWay, OrderStyle::PostOnly).unwrap();
    let b = prepare_order(&intent(), &filters(), PositionMode::OneWay, OrderStyle::PostOnly).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.path, "/papi/v1/um/order");
    assert_eq!(a.get("symbol"), Some("BTCUSDC"));
    assert_eq!(a.get("side"), Some("BUY"));
    assert_eq!(a.get("type"), Some("LIMIT"));
    assert_eq!(a.get("positionSide"), Some("BOTH"));
    assert_eq!(a.get("timeInForce"), Some("GTX"));
    assert_eq!(a.get("reduceOnly"), Some("false"));
    assert_eq!(a.get("quantity"), Some("0.010"));
    assert_eq!(a.get("newClientOrderId"), Some(a.client_id.as_str()));
    assert_eq!(a.client_id.len(), 28);
    assert!(a.get("timestamp").is_none());
    assert!(a.get("signature").is_none());
}

#[test]
fn invalid_exchange_info_and_wrong_contract_fail_closed() {
    let mut f = filters();
    f.symbol_trading = false;
    assert!(prepare_order(&intent(), &f, PositionMode::OneWay, OrderStyle::PostOnly).is_err());
    f.symbol_trading = true;
    f.tick_size = "0".to_owned();
    assert_eq!(
        prepare_order(&intent(), &f, PositionMode::OneWay, OrderStyle::PostOnly),
        Err(ProtocolError::InvalidFilters)
    );
    let mut wrong = intent();
    wrong.asset = "BTCUSDT".to_owned();
    assert_eq!(
        prepare_order(&wrong, &filters(), PositionMode::OneWay, OrderStyle::PostOnly),
        Err(ProtocolError::WrongInstrument)
    );
}

#[test]
fn size_price_filters_and_notional_are_enforced() {
    let mut order = intent();
    order.quantity = Decimal::from_str("0.0105").unwrap();
    assert_eq!(
        prepare_order(&order, &filters(), PositionMode::OneWay, OrderStyle::PostOnly),
        Err(ProtocolError::InvalidQuantity)
    );
    order.quantity = Decimal::from_str("0.010").unwrap();
    order.limit_price = Some(Decimal::from_str("80000.05").unwrap());
    assert_eq!(
        prepare_order(&order, &filters(), PositionMode::OneWay, OrderStyle::PostOnly),
        Err(ProtocolError::InvalidPrice)
    );
    order.limit_price = Some(Decimal::from_str("100.0").unwrap());
    assert_eq!(
        prepare_order(&order, &filters(), PositionMode::OneWay, OrderStyle::PostOnly),
        Err(ProtocolError::InvalidQuantity)
    );
}

#[test]
fn hedge_mode_unverified_mode_and_exposure_increasing_market_are_refused() {
    for mode in [PositionMode::Hedge, PositionMode::Unverified] {
        assert_eq!(
            prepare_order(&intent(), &filters(), mode, OrderStyle::PostOnly),
            Err(ProtocolError::UnsupportedPositionMode)
        );
    }
    assert_eq!(
        prepare_order(&intent(), &filters(), PositionMode::OneWay, OrderStyle::ReduceOnlyMarket),
        Err(ProtocolError::UnsupportedExposure)
    );
}

#[test]
fn reduction_market_has_no_price_or_tif_and_requires_owned_risk_path() {
    let mut order = intent();
    order.effect = ExposureEffect::ReduceOnly;
    order.side = Side::Sell;
    order.limit_price = None;
    let prepared = prepare_order(
        &order,
        &filters(),
        PositionMode::OneWay,
        OrderStyle::ReduceOnlyMarket,
    )
    .unwrap();
    assert_eq!(prepared.get("side"), Some("SELL"));
    assert_eq!(prepared.get("type"), Some("MARKET"));
    assert_eq!(prepared.get("reduceOnly"), Some("true"));
    assert!(prepared.get("price").is_none());
    assert!(prepared.get("timeInForce").is_none());
}
