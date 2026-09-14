use super::{IbkrConfig, sdk};
use async_trait::async_trait;
use futures::StreamExt;
use pg_marketdata::{
    InstrumentDescriptor, ProductType, UniverseError, UniverseProvider, UniverseSnapshot,
};
use pg_types::Venue;
use rust_decimal::Decimal;
use sdk::contracts::SecurityType;
use sdk::scanner::ScannerSubscription;
use sdk::subscriptions::SubscriptionItemStreamExt;
use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const IBKR_SCANNER_MAX_ROWS: i32 = 50;

#[derive(Debug, Clone)]
pub struct IbkrScannerConfig {
    pub number_of_rows: i32,
    pub instrument: String,
    pub location_code: String,
    pub scan_code: String,
    pub above_price: Option<f64>,
    pub below_price: Option<f64>,
    pub above_volume: Option<i32>,
    pub stock_type_filter: Option<String>,
    pub timeout: Duration,
}

impl Default for IbkrScannerConfig {
    fn default() -> Self {
        Self {
            number_of_rows: IBKR_SCANNER_MAX_ROWS,
            instrument: "STK".into(),
            location_code: "STK.US.MAJOR".into(),
            scan_code: "MOST_ACTIVE".into(),
            above_price: None,
            below_price: None,
            above_volume: None,
            stock_type_filter: None,
            timeout: Duration::from_secs(15),
        }
    }
}

pub struct IbkrUniverseProvider {
    client: Arc<sdk::Client>,
    scanner: IbkrScannerConfig,
}

impl IbkrUniverseProvider {
    pub async fn connect(
        config: &IbkrConfig,
        scanner: IbkrScannerConfig,
    ) -> Result<Self, UniverseError> {
        validate_scanner_config(&scanner)?;
        let client = sdk::Client::connect(&config.gateway_addr, config.client_id)
            .await
            .map_err(|error| UniverseError::Transport(error.to_string()))?;
        Ok(Self {
            client: Arc::new(client),
            scanner,
        })
    }

    async fn snapshot(&self) -> Result<UniverseSnapshot, UniverseError> {
        let scanner = ScannerSubscription {
            number_of_rows: self.scanner.number_of_rows,
            instrument: Some(self.scanner.instrument.clone()),
            location_code: Some(self.scanner.location_code.clone()),
            scan_code: Some(self.scanner.scan_code.clone()),
            above_price: self.scanner.above_price,
            below_price: self.scanner.below_price,
            above_volume: self.scanner.above_volume,
            stock_type_filter: self.scanner.stock_type_filter.clone(),
            ..Default::default()
        };
        let filters = Vec::new();
        let subscription = self
            .client
            .scanner_subscription(&scanner, &filters)
            .await
            .map_err(|error| UniverseError::Transport(error.to_string()))?;
        let mut stream = subscription.filter_data();
        let next = tokio::time::timeout(self.scanner.timeout, stream.next())
            .await
            .map_err(|_| {
                UniverseError::Transport(format!(
                    "IBKR scanner timed out after {}ms",
                    self.scanner.timeout.as_millis()
                ))
            })?
            .ok_or_else(|| UniverseError::Transport("IBKR scanner stream ended".into()))?
            .map_err(|error| UniverseError::Transport(error.to_string()))?;

        let instruments = next
            .into_iter()
            .map(|row| descriptor(row.contract_details))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(UniverseSnapshot {
            venue: Venue::InteractiveBrokers,
            discovered_at_ns: now_ns()?,
            instruments,
        })
    }
}

#[async_trait]
impl UniverseProvider for IbkrUniverseProvider {
    fn venue(&self) -> Venue {
        Venue::InteractiveBrokers
    }

    async fn discover(&self) -> Result<UniverseSnapshot, UniverseError> {
        self.snapshot().await
    }
}

fn validate_scanner_config(config: &IbkrScannerConfig) -> Result<(), UniverseError> {
    if !(1..=IBKR_SCANNER_MAX_ROWS).contains(&config.number_of_rows) {
        return Err(UniverseError::Conversion(format!(
            "IBKR scanner number_of_rows must be 1..={IBKR_SCANNER_MAX_ROWS}"
        )));
    }
    if config.instrument.trim().is_empty()
        || config.location_code.trim().is_empty()
        || config.scan_code.trim().is_empty()
    {
        return Err(UniverseError::Conversion(
            "IBKR scanner instrument/location_code/scan_code must be non-empty".into(),
        ));
    }
    if config.timeout.is_zero() {
        return Err(UniverseError::Conversion(
            "IBKR scanner timeout must be positive".into(),
        ));
    }
    Ok(())
}

fn descriptor(
    details: sdk::contracts::ContractDetails,
) -> Result<InstrumentDescriptor, UniverseError> {
    let contract = details.contract;
    let symbol = contract.symbol.to_string();
    let currency = contract.currency.to_string();
    let exchange = contract.exchange.to_string();
    let primary_exchange = contract.primary_exchange.to_string();
    let product_type = match contract.security_type {
        SecurityType::Stock if details.stock_type.eq_ignore_ascii_case("ETF") => ProductType::Etf,
        SecurityType::Stock => ProductType::Stock,
        SecurityType::Future | SecurityType::ContinuousFuture => ProductType::Future,
        SecurityType::ForexPair => ProductType::Forex,
        _ => ProductType::Unknown,
    };
    let tick_size = positive_decimal(details.min_tick)?;
    let min_size = optional_positive_decimal(details.min_size)?;
    let lot_size = optional_positive_decimal(details.size_increment)?;

    Ok(InstrumentDescriptor {
        venue: Venue::InteractiveBrokers,
        symbol: symbol.clone(),
        product_type,
        venue_instrument_id: (contract.contract_id > 0).then(|| contract.contract_id.to_string()),
        base: Some(symbol),
        quote: (!currency.is_empty()).then(|| currency.clone()),
        exchange: (!exchange.is_empty()).then_some(exchange),
        primary_exchange: (!primary_exchange.is_empty()).then_some(primary_exchange),
        currency: (!currency.is_empty()).then_some(currency),
        size_decimals: None,
        tick_size,
        lot_size,
        min_size,
        mark_price: None,
        mid_price: None,
        spread_bps: None,
        day_notional_volume: None,
        open_interest: None,
        funding_rate: None,
        max_leverage: None,
        tradable: contract.contract_id > 0,
    })
}

fn optional_positive_decimal(value: Option<f64>) -> Result<Option<Decimal>, UniverseError> {
    match value {
        Some(value) => positive_decimal(value),
        None => Ok(None),
    }
}

fn positive_decimal(value: f64) -> Result<Option<Decimal>, UniverseError> {
    if !value.is_finite() || value <= 0.0 {
        return Ok(None);
    }
    Decimal::from_str(&value.to_string())
        .map(Some)
        .map_err(|error| UniverseError::Conversion(error.to_string()))
}

fn now_ns() -> Result<u64, UniverseError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .map_err(|error| UniverseError::Conversion(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_config_rejects_invalid_row_counts() {
        let zero = IbkrScannerConfig {
            number_of_rows: 0,
            ..Default::default()
        };
        assert!(validate_scanner_config(&zero).is_err());
        let too_many = IbkrScannerConfig {
            number_of_rows: IBKR_SCANNER_MAX_ROWS + 1,
            ..Default::default()
        };
        assert!(validate_scanner_config(&too_many).is_err());
        let maximum = IbkrScannerConfig::default();
        assert!(validate_scanner_config(&maximum).is_ok());
    }

    #[test]
    fn optional_size_metadata_stays_optional() {
        assert_eq!(optional_positive_decimal(None).unwrap(), None);
        assert_eq!(optional_positive_decimal(Some(0.0)).unwrap(), None,);
        assert_eq!(
            optional_positive_decimal(Some(100.0)).unwrap(),
            Some(Decimal::from(100)),
        );
    }
}
