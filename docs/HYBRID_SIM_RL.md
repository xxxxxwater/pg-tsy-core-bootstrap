# Hybrid Python/Rust simulation and training plane

Python stays the strategy/research surface: feature work, debugging, notebooks, RL/ES policy code and batched experiments. Rust owns deterministic execution semantics, OMS-compatible order behavior and future low-latency replay.

The pg-sim crate is the reference matching kernel. The Python BatchMarketEnv is intentionally a lightweight vectorized training loop; it does not pretend to model live exchange microstructure.

Implemented Rust simulator semantics include IOC, FOK, GTC, GTD, DAY, AT_THE_OPEN, AT_THE_CLOSE, post-only, reduce-only, iceberg display quantity, OCO, OTO and OUO. OUO is defined here as one-updates-other quantity: each fill reduces the peer total quantity by the same amount.

None of these simulator capabilities automatically enables a live order type. Binance PM, Hyperliquid and IBKR adapters must advertise and validate their own capabilities. Unsupported semantics must fail closed rather than silently downgrade.

pg-core already uses Tokio. The production binary also installs mimalloc as its global allocator in this change. Performance claims must be based on benchmark evidence; no fixed interactions-per-second promise is asserted.

For RL/ES, use BatchMarketEnv to run many environments per Python call and reserve pg-sim for exchange-sensitive execution validation. A future PyO3 or Arrow shared-memory bridge can remove the remaining Python/Rust boundary once benchmark data justifies it.
