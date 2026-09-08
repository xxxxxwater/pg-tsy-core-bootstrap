# Local-only ML training and parameter optimization

Training is intentionally separated from the AWS live runtime.

## Why

A production trading process should not consume GPU memory, mutate model weights or search parameters while it is managing real orders. The live process consumes immutable, versioned artifacts produced by an offline/local workflow.

## Local profile

Install:

```bash
cd research
pip install -e '.[dev,train]'
```

The trainer detects accelerators in this order:

1. CUDA GPU;
2. Apple MPS;
3. CPU.

## Validation before artifact promotion

Minimum checks:

- chronological train/validation/test split;
- purged walk-forward folds with an embargo around boundaries;
- no future feature leakage;
- fees/spread/slippage in the objective;
- parameter stability across folds/regimes;
- performance concentration check by asset/day/regime;
- compare against simple linear/tree/DeepLOB-style baselines before adding complexity;
- freeze dataset snapshot, feature-set version and code commit in the artifact manifest.

A good in-sample optimum with unstable neighboring parameters is rejected.

## Hyperparameter search

`pg_tsy.tuning` provides walk-forward split utilities and an optional Optuna bridge. Optimize an out-of-sample robustness objective, not raw training accuracy or backtest PnL.

Suggested objective:

```text
median_oos_sharpe
- drawdown_penalty
- turnover_cost_penalty
- fold_dispersion_penalty
- parameter_instability_penalty
```

## Deployment

Only these objects leave the research machine:

- immutable model artifact (for example ONNX / TorchScript where appropriate);
- feature-set manifest;
- normalization parameters;
- model/version metadata;
- validation report/hash.

The AWS live image does not install the `train` extra.
