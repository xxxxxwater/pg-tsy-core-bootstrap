# Contracts

Cross-language messages are versioned here. `signal.v1.json` is the initial research → live-core boundary.

Rules:

- Add fields compatibly where possible.
- Breaking changes create a new contract version.
- Timestamps are UTC Unix nanoseconds unless a schema says otherwise.
- Decimal strings are used for prices/quantities when exact venue precision matters.
- Every signal is immutable and uniquely identified.
