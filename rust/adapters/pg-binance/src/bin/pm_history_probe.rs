//! Explicitly approved, read-only probe of signed Portfolio Margin UM trade history.
//! This never places or cancels orders, mutates OMS, persists a cursor, or lifts
//! SAFE_HOLD. The operator must independently verify segregated account rights
//! and establish the original history anchor before using any result.

use std::{env, process::ExitCode};

use pg_binance::{rest_transport::BinanceRestClient, trade_history::TradePage};

const LIMIT: u16 = 1000;
const MAX_PAGES: usize = 32;

fn permitted(approval: &str, mode: &str, live: &str, scope: &str, key: &str, secret: &str, anchor: u64) -> bool {
    approval == "APPROVE_ISOLATED_READ_ONLY_HISTORY_PROBE"
        && mode == "shadow"
        && live == "false"
        && !scope.trim().is_empty()
        && !key.is_empty()
        && !secret.is_empty()
        && anchor > 0
        && anchor <= i64::MAX as u64
}

/// Verify the cross-page cursor and full/short-page claims before any cursor
/// can be used for the next request. Trade-ID gaps are legal; overlaps are not.
fn advance(cursor: u64, page: &TradePage, limit: usize) -> Result<Option<u64>, &'static str> {
    if limit == 0
        || page.trades.len() > limit
        || page.short_page != (page.trades.len() < limit)
        || page.trades.first().is_some_and(|trade| trade.trade_id < cursor)
    {
        return Err("invalid or incomplete UM trade-history page");
    }
    let next = page.trades.last()
        .map(|trade| trade.trade_id.checked_add(1).ok_or("trade-history cursor overflow"))
        .transpose()?
        .unwrap_or(cursor);
    if page.next_from_id != Some(next) || (!page.short_page && next <= cursor) {
        return Err("inconsistent UM history cursor; SAFE_HOLD required");
    }
    if page.short_page {
        Ok(None)
    } else {
        Ok(Some(next))
    }
}

async fn run() -> Result<(), &'static str> {
    let approval = env::var("PG_PM_HISTORY_PROBE_APPROVAL").unwrap_or_default();
    let mode = env::var("PG_RUN_MODE").unwrap_or_default();
    let live = env::var("PG_LIVE_TRADING").unwrap_or_default();
    let scope = env::var("PG_ISOLATED_ACCOUNT_SCOPE").unwrap_or_default();
    let key = env::var("PG_BINANCE_PM_API_KEY").unwrap_or_default();
    let secret = env::var("PG_BINANCE_PM_API_SECRET").unwrap_or_default();
    let anchor = env::var("PG_PM_HISTORY_FROM_ID")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default();
    if !permitted(&approval, &mode, &live, &scope, &key, &secret, anchor) {
        return Err("signed PM history probe requires segregated read-only approval and trusted anchor");
    }
    let client = BinanceRestClient::new(key, secret, false)
        .map_err(|_| "read-only PM signer initialization failed")?;
    let mut cursor = anchor;
    let mut observed = 0_usize;
    for page_number in 1..=MAX_PAGES {
        let page = client.user_trades_page(Some(cursor), LIMIT).await
            .map_err(|_| "signed PM trade-history read or decode failed; SAFE_HOLD required")?;
        observed = observed.checked_add(page.trades.len())
            .ok_or("UM trade-history count overflow")?;
        match advance(cursor, &page, usize::from(LIMIT))? {
            Some(next) => cursor = next,
            None => {
                // This is only a complete interval after an independently
                // established anchor, NOT full account-history/OMS evidence.
                println!(
                    "signed_history_read=true pages={} trades={} anchor_operator_verified=false oms_updates=0 order_posts=0 cursor_persisted=false safe_hold_required=true",
                    page_number, observed
                );
                return Ok(());
            }
        }
    }
    Err("bounded signed UM history exhausted without a terminal short page; SAFE_HOLD required")
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_binance::trade_history::UmTrade;
    use pg_types::Side;
    use rust_decimal::Decimal;

    fn trade(id: u64) -> UmTrade {
        UmTrade {
            symbol: "BTCUSDC".into(),
            trade_id: id,
            venue_order_id: "123".into(),
            side: Side::Buy,
            quantity: Decimal::ONE,
            price: Decimal::from(100),
            quote_quantity: Decimal::from(100),
            commission: Decimal::ZERO,
            commission_asset: "USDC".into(),
            realized_pnl: Decimal::ZERO,
            trade_time_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn never_network_without_explicit_independent_operator_approval() {
        let approved = "APPROVE_ISOLATED_READ_ONLY_HISTORY_PROBE";
        assert!(!permitted("", "shadow", "false", "segregated", "key", "secret", 1));
        assert!(!permitted(approved, "live", "false", "segregated", "key", "secret", 1));
        assert!(!permitted(approved, "shadow", "true", "segregated", "key", "secret", 1));
        assert!(!permitted(approved, "shadow", "false", "", "key", "secret", 1));
        assert!(!permitted(approved, "shadow", "false", "segregated", "key", "", 1));
        assert!(!permitted(approved, "shadow", "false", "segregated", "key", "secret", 0));
        assert!(permitted(approved, "shadow", "false", "segregated", "key", "secret", 1));
    }

    #[test]
    fn full_page_advances_and_short_terminal_page_stops() {
        let full = TradePage {
            trades: vec![trade(9), trade(12)],
            next_from_id: Some(13),
            short_page: false,
        };
        assert_eq!(advance(9, &full, 2).unwrap(), Some(13));
        let final_page = TradePage {
            trades: vec![trade(14)],
            next_from_id: Some(15),
            short_page: true,
        };
        assert_eq!(advance(13, &final_page, 2).unwrap(), None);
    }

    #[test]
    fn duplicated_old_trades_bad_cursor_and_false_short_pages_fail_closed() {
        let page = TradePage {
            trades: vec![trade(9), trade(10)],
            next_from_id: Some(11),
            short_page: false,
        };
        assert!(advance(11, &page, 2).is_err());
        let bad_cursor = TradePage { next_from_id: Some(99), ..page.clone() };
        assert!(advance(9, &bad_cursor, 2).is_err());
        let false_short = TradePage { short_page: true, ..page };
        assert!(advance(9, &false_short, 2).is_err());
    }
}
