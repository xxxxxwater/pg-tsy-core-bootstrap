use super::{HyperliquidNetwork, sdk};
use async_trait::async_trait;
use pg_marketdata::{
    InstrumentDescriptor, ProductType, UniverseError, UniverseProvider, UniverseSnapshot,
};
use pg_types::Venue;
use rust_decimal::Decimal;
use std::{
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct HyperliquidUniverseProvider {
    info: sdk::InfoClient,
}

impl HyperliquidUniverseProvider {
    pub async fn connect(network: HyperliquidNetwork) -> Result<Self, UniverseError> {
        let base_url = match network {
            HyperliquidNetwork::Mainnet => sdk::BaseUrl::Mainnet,
            HyperliquidNetwork::Testnet => sdk::BaseUrl::Testnet,
        };
        let info = sdk::InfoClient::new(None, Some(base_url))
            .await
            .map_err(|error| UniverseError::Transport(error.to_string()))?;
        Ok(Self { info })
    }

    async fn snapshot(&self) -> Result<UniverseSnapshot, UniverseError> {
        let (meta, contexts) = self
            .info
            .meta_and_asset_contexts()
            .await
            .map_err(|error| UniverseError::Transport(error.to_string()))?;
        if meta.universe.len() != contexts.len() {
            return Err(UniverseError::Conversion(format!(
                "Hyperliquid metadata/context length mismatch: {} != {}",
                meta.universe.len(),
                contexts.len()
            )));
        }

        let mut instruments = Vec::with_capacity(meta.universe.len());
        for (index, (asset, context)) in meta.universe.into_iter().zip(contexts).enumerate() {
            let mark_price = decimal("markPx", &context.mark_px)?;
            let mid_price = context
                .mid_px
                .as_deref()
                .map(|value| decimal("midPx", value))
                .transpose()?;
            let day_notional_volume = decimal("dayNtlVlm", &context.day_ntl_vlm)?;
            let open_interest = decimal("openInterest", &context.open_interest)?;
            let funding_rate = decimal("funding", &context.funding)?;
            let max_leverage = u32::try_from(asset.max_leverage).map_err(|_| {
                UniverseError::Conversion(format!(
                    "Hyperliquid max leverage overflows u32 for {}",
                    asset.name
                ))
            })?;

            // The pinned Hyperliquid metadata response does not carry BBO spread or
            // a separate disabled flag. Do not invent those values: spread stays
            // None and tradability requires a positive venue mark.
            let tradable = mark_price > Decimal::ZERO;
            instruments.push(InstrumentDescriptor {
                venue: Venue::Hyperliquid,
                symbol: asset.name.clone(),
                product_type: ProductType::Perpetual,
                venue_instrument_id: Some(index.to_string()),
                base: Some(asset.name),
                quote: Some("USDC".into()),
                exchange: Some("HYPERLIQUID".into()),
                primary_exchange: None,
                currency: Some("USDC".into()),
                size_decimals: Some(asset.sz_decimals),
                tick_size: None,
                lot_size: None,
                min_size: None,
                mark_price: Some(mark_price),
                mid_price,
                spread_bps: None,
                day_notional_volume: Some(day_notional_volume),
                open_interest: Some(open_interest),
                funding_rate: Some(funding_rate),
                max_leverage: Some(max_leverage),
                tradable,
            });
        }

        Ok(UniverseSnapshot {
            venue: Venue::Hyperliquid,
            discovered_at_ns: now_ns()?,
            instruments,
        })
    }
}

#[async_trait]
impl UniverseProvider for HyperliquidUniverseProvider {
    fn venue(&self) -> Venue {
        Venue::Hyperliquid
    }

    async fn discover(&self) -> Result<UniverseSnapshot, UniverseError> {
        self.snapshot().await
    }
}

fn decimal(field: &str, value: &str) -> Result<Decimal, UniverseError> {
    Decimal::from_str(value).map_err(|error| {
        UniverseError::Conversion(format!("invalid Hyperliquid {field}={value}: {error}"))
    })
}

fn now_ns() -> Result<u64, UniverseError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .map_err(|error| UniverseError::Conversion(error.to_string()))
}
