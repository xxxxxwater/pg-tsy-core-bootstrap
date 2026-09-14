use anyhow::{Context, Result, bail};
use pg_marketdata::{InstrumentDescriptor, UniverseProvider, UniverseSnapshot};
use pg_strategy::registry::StrategyRegistry;
use pg_types::{AssetKey, Venue};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
};
#[cfg(feature = "ibkr-marketdata")]
use std::time::Duration;

#[cfg(feature = "hyperliquid-marketdata")]
use pg_hyperliquid::{HyperliquidNetwork, HyperliquidUniverseProvider};
#[cfg(feature = "ibkr-marketdata")]
use pg_ibkr::{IbkrConfig, IbkrScannerConfig, IbkrUniverseProvider};

#[derive(Debug, Clone, Default)]
pub struct DynamicUniverseResolution {
    /// Full provider metadata keyed by venue+symbol. This includes scanner rows that
    /// were not selected so pinned assets can still reuse known conId/contract data.
    pub descriptors: BTreeMap<AssetKey, InstrumentDescriptor>,
    /// Assets that require venue adapters/recovery even when they are not strategy
    /// candidates. Manual and Unknown positions intentionally live here, not in the
    /// strategy registry.
    pub operational_pins: BTreeSet<AssetKey>,
    /// Assets selected by dynamic templates, including explicit StrategyOwned pins.
    pub strategy_assets: BTreeSet<AssetKey>,
}

pub async fn refresh_dynamic_universe(
    registry: &mut StrategyRegistry,
    strategy_pins: &BTreeSet<AssetKey>,
    extra_operational_pins: &BTreeSet<AssetKey>,
) -> Result<DynamicUniverseResolution> {
    let templates = registry.dynamic_templates();
    if templates.is_empty() {
        return Ok(DynamicUniverseResolution {
            descriptors: BTreeMap::new(),
            operational_pins: extra_operational_pins.clone(),
            strategy_assets: BTreeSet::new(),
        });
    }

    let required_venues = templates
        .iter()
        .flat_map(|template| {
            template
                .definition
                .universe
                .dynamic_sources()
                .iter()
                .map(|source| source.venue)
        })
        .collect::<BTreeSet<_>>();

    let mut snapshots = BTreeMap::<Venue, UniverseSnapshot>::new();
    let mut operational_pins = extra_operational_pins.clone();

    if required_venues.contains(&Venue::Hyperliquid) {
        #[cfg(feature = "hyperliquid-marketdata")]
        {
            let provider = HyperliquidUniverseProvider::connect(hyperliquid_network()?)
                .await
                .context("failed to connect Hyperliquid universe provider")?;
            let account = required_env("HYPERLIQUID_ACCOUNT_ADDRESS")?;
            for asset in provider
                .account_assets(&account)
                .await
                .context("failed to read Hyperliquid account assets for universe pinning")?
            {
                operational_pins.insert(AssetKey::new(Venue::Hyperliquid, asset));
            }
            snapshots.insert(
                Venue::Hyperliquid,
                provider
                    .discover()
                    .await
                    .context("Hyperliquid universe discovery failed")?,
            );
        }
        #[cfg(not(feature = "hyperliquid-marketdata"))]
        bail!("dynamic Hyperliquid universe requested without Hyperliquid SDK support");
    }

    if required_venues.contains(&Venue::InteractiveBrokers) {
        #[cfg(feature = "ibkr-marketdata")]
        {
            let mut config = ibkr_config_from_env()?;
            config.client_id = env_i32(
                "IBKR_SCANNER_CLIENT_ID",
                config.client_id.saturating_add(5_000),
            )?;
            let provider = IbkrUniverseProvider::connect(&config, ibkr_scanner_from_env()?)
                .await
                .context("failed to connect IBKR universe provider")?;
            for asset in provider
                .account_assets(config.account.as_deref())
                .await
                .context("failed to read IBKR account assets for universe pinning")?
            {
                operational_pins.insert(AssetKey::new(Venue::InteractiveBrokers, asset));
            }
            snapshots.insert(
                Venue::InteractiveBrokers,
                provider
                    .discover()
                    .await
                    .context("IBKR scanner discovery failed")?,
            );
        }
        #[cfg(not(feature = "ibkr-marketdata"))]
        bail!("dynamic IBKR universe requested without ibkr-marketdata/SDK support");
    }

    if required_venues.contains(&Venue::BinancePm) {
        bail!("dynamic BINANCE_PM universe remains fail-closed: no production adapter exists");
    }

    let mut descriptors = BTreeMap::new();
    for snapshot in snapshots.values() {
        for descriptor in &snapshot.instruments {
            descriptors.insert(
                AssetKey::new(descriptor.venue, descriptor.symbol.clone()),
                descriptor.clone(),
            );
        }
    }

    let mut strategy_assets = BTreeSet::new();
    for template in templates {
        let mut selected = Vec::<AssetKey>::new();
        for source in template.definition.universe.dynamic_sources() {
            let snapshot = snapshots.get(&source.venue).ok_or_else(|| {
                anyhow::anyhow!("missing discovered snapshot for {:?}", source.venue)
            })?;
            for descriptor in source.filter()?.apply(snapshot) {
                let key = AssetKey::new(source.venue, descriptor.symbol);
                if !selected.contains(&key) {
                    selected.push(key);
                }
            }
            for key in strategy_pins.iter().filter(|key| key.venue == source.venue) {
                if !selected.contains(key) {
                    selected.push(key.clone());
                }
            }
        }
        let ids = registry
            .apply_dynamic_instruments(&template.path, &selected)
            .with_context(|| {
                format!(
                    "failed to materialize dynamic strategy template {}",
                    template.path.display()
                )
            })?;
        strategy_assets.extend(selected);
        tracing::info!(
            template = %template.path.display(),
            strategy_instances = ids.len(),
            "dynamic universe materialized"
        );
    }

    operational_pins.extend(strategy_assets.iter().cloned());
    Ok(DynamicUniverseResolution {
        descriptors,
        operational_pins,
        strategy_assets,
    })
}

fn required_env(name: &'static str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("{name} is required"))?;
    if value.trim().is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(value)
}

#[cfg(feature = "hyperliquid-marketdata")]
fn hyperliquid_network() -> Result<HyperliquidNetwork> {
    match env::var("PG_HYPERLIQUID_NETWORK")
        .or_else(|_| env::var("HYPERLIQUID_NETWORK"))
        .unwrap_or_else(|_| "mainnet".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "mainnet" => Ok(HyperliquidNetwork::Mainnet),
        "testnet" => Ok(HyperliquidNetwork::Testnet),
        other => bail!("invalid Hyperliquid network {other}; expected mainnet or testnet"),
    }
}

#[cfg(feature = "ibkr-marketdata")]
fn ibkr_config_from_env() -> Result<IbkrConfig> {
    Ok(IbkrConfig {
        gateway_addr: env::var("IBKR_GATEWAY_ADDR").unwrap_or_else(|_| "127.0.0.1:4002".into()),
        client_id: env_i32("IBKR_CLIENT_ID", 17)?,
        account: env::var("IBKR_ACCOUNT")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        market_depth_rows: env_i32("IBKR_MARKET_DEPTH_ROWS", 5)?,
    })
}

#[cfg(feature = "ibkr-marketdata")]
fn ibkr_scanner_from_env() -> Result<IbkrScannerConfig> {
    Ok(IbkrScannerConfig {
        number_of_rows: env_i32("IBKR_SCANNER_ROWS", 50)?,
        instrument: env::var("IBKR_SCANNER_INSTRUMENT").unwrap_or_else(|_| "STK".into()),
        location_code: env::var("IBKR_SCANNER_LOCATION")
            .unwrap_or_else(|_| "STK.US.MAJOR".into()),
        scan_code: env::var("IBKR_SCANNER_CODE").unwrap_or_else(|_| "MOST_ACTIVE".into()),
        above_price: env_optional_f64("IBKR_SCANNER_ABOVE_PRICE")?,
        below_price: env_optional_f64("IBKR_SCANNER_BELOW_PRICE")?,
        above_volume: env_optional_i32("IBKR_SCANNER_ABOVE_VOLUME")?,
        stock_type_filter: env::var("IBKR_SCANNER_STOCK_TYPE")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        timeout: Duration::from_millis(env_u64("IBKR_SCANNER_TIMEOUT_MS", 15_000)?),
    })
}

#[cfg(feature = "ibkr-marketdata")]
fn env_i32(name: &'static str, default: i32) -> Result<i32> {
    env::var(name)
        .ok()
        .map(|value| value.parse::<i32>().with_context(|| format!("invalid {name}")))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

#[cfg(feature = "ibkr-marketdata")]
fn env_optional_i32(name: &'static str) -> Result<Option<i32>> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.parse::<i32>().with_context(|| format!("invalid {name}")))
        .transpose()
}

#[cfg(feature = "ibkr-marketdata")]
fn env_optional_f64(name: &'static str) -> Result<Option<f64>> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.parse::<f64>().with_context(|| format!("invalid {name}")))
        .transpose()
}

#[cfg(feature = "ibkr-marketdata")]
fn env_u64(name: &'static str, default: u64) -> Result<u64> {
    env::var(name)
        .ok()
        .map(|value| value.parse::<u64>().with_context(|| format!("invalid {name}")))
        .transpose()
        .map(|value| value.unwrap_or(default))
}