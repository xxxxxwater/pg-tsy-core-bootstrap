//! Reusable, venue-neutral technical factor formulas.
//!
//! These functions operate on normalized values and are intentionally independent
//! of Binance, Hyperliquid, IBKR, Freqtrade, pandas or TA-Lib.

pub fn ema_last(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period || values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let seed = values[..period].iter().sum::<f64>() / period as f64;
    let alpha = 2.0 / (period as f64 + 1.0);
    Some(
        values[period..]
            .iter()
            .fold(seed, |ema, value| alpha * value + (1.0 - alpha) * ema),
    )
}

pub fn momentum_bps(values: &[f64], lookback: usize) -> Option<f64> {
    if lookback == 0 || values.len() <= lookback {
        return None;
    }
    let current = *values.last()?;
    let previous = values[values.len() - 1 - lookback];
    if !current.is_finite() || !previous.is_finite() || previous <= 0.0 {
        return None;
    }
    Some((current / previous - 1.0) * 10_000.0)
}

pub fn ewo_percent(values: &[f64], fast_period: usize, slow_period: usize) -> Option<f64> {
    let close = *values.last()?;
    if close <= 0.0 {
        return None;
    }
    let fast = ema_last(values, fast_period)?;
    let slow = ema_last(values, slow_period)?;
    Some((fast - slow) / close * 100.0)
}

pub fn volume_ratio(volumes: &[f64], prior_window: usize) -> Option<f64> {
    if prior_window == 0 || volumes.len() <= prior_window {
        return None;
    }
    let current = *volumes.last()?;
    let prior = &volumes[volumes.len() - 1 - prior_window..volumes.len() - 1];
    if prior.iter().any(|value| !value.is_finite() || *value < 0.0) || current < 0.0 {
        return None;
    }
    let mean = prior.iter().sum::<f64>() / prior_window as f64;
    (mean > 0.0).then_some(current / mean)
}

pub fn rsi_wilder(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() <= period || values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let start = values.len() - period - 1;
    let mut gains = 0.0;
    let mut losses = 0.0;
    for pair in values[start..].windows(2) {
        let delta = pair[1] - pair[0];
        if delta >= 0.0 {
            gains += delta;
        } else {
            losses -= delta;
        }
    }
    if losses == 0.0 {
        return Some(100.0);
    }
    let rs = (gains / period as f64) / (losses / period as f64);
    Some(100.0 - 100.0 / (1.0 + rs))
}

pub fn vwap(prices: &[f64], volumes: &[f64]) -> Option<f64> {
    if prices.len() != volumes.len() || prices.is_empty() {
        return None;
    }
    let mut quote = 0.0;
    let mut volume = 0.0;
    for (price, size) in prices.iter().zip(volumes) {
        if !price.is_finite() || !size.is_finite() || *price <= 0.0 || *size < 0.0 {
            return None;
        }
        quote += price * size;
        volume += size;
    }
    (volume > 0.0).then_some(quote / volume)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewo_matches_strategy_formula_shape() {
        let closes: Vec<f64> = (1..=250).map(|value| value as f64).collect();
        let ewo = ewo_percent(&closes, 50, 200).unwrap();
        assert!(ewo > 0.0);
    }

    #[test]
    fn volume_ratio_uses_current_against_prior_window() {
        let mut volumes = vec![10.0; 24];
        volumes.push(15.0);
        assert_eq!(volume_ratio(&volumes, 24), Some(1.5));
    }

    #[test]
    fn missing_history_returns_none() {
        assert!(momentum_bps(&[100.0], 1).is_none());
        assert!(rsi_wilder(&[100.0; 14], 14).is_none());
    }
}
