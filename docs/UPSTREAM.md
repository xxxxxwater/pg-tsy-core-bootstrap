# Upstream projects to study, wrap or reuse

Do not vendor these projects unless there is a concrete reason. Prefer stable package/SDK boundaries.

Research/data:

- Polars — columnar factor/data computation.
- Apache Arrow / PyArrow — interchange and Parquet IO.
- Microsoft Qlib — research workflow ideas.
- vectorbt — fast research parameter/factor screening ideas.
- AlphaGen — formulaic alpha discovery research.

ML / LOB:

- PyTorch and JAX.
- LOBFrame, DeepLOB and TLOB — baseline/research methodology.
- JAX-LOB / AlphaTrade — simulator/execution research.

Trading/execution:

- NautilusTrader — trading-domain abstractions and Rust/Python architecture reference.
- Binance official Rust connector — Binance and Portfolio Margin API boundary.
- Hyperliquid official Rust SDK — Hyperliquid boundary.
- hftbacktest — latency/queue-aware replay methodology.
- Barter — Rust-native trading architecture reference.

Rule: upstream inspiration does not change repository ownership boundaries. Exchange SDK response types must not leak into core domain crates.
