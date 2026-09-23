# Jev -> Laya System-One failover

Status: **research/shadow failover only; no live-order authority.**

## Why Laya

PG-TSY keeps model judgment outside the deterministic execution hot path. The primary
System-One provider remains TypeSafe Jev. If Jev is unavailable, times out, returns an
invalid schema, or returns a response after the snapshot TTL, the research client may
attempt Laya exactly once on the same still-fresh snapshot.

Upstream: https://github.com/NandhaKishorM/laya

Laya is Apache-2.0 and exposes a Jev-compatible `POST /v1/systemone` HTTP surface. The
integration therefore does not vendor Laya into the Rust OMS and does not create a
second order dispatcher.

Audited upstream main when this adapter was added:

`ec8409e542941bb4bb649d5fec00d4cec96ae024`

That SHA is documentation provenance, **not** proof that a deployed Laya instance is
running that build. A deployment must set its own `LAYA_BUILD_ID`.

## Contract

```
immutable BTCUSDC feature snapshot
        |
        +--> Jev (one bounded request)
        |       |
        |       +--> valid + fresh -> advisory observation
        |       |
        |       +--> unavailable/invalid/late
        |                    |
        |                    v
        +---------------> Laya (one bounded request)
                                |
                                +--> valid + fresh -> advisory observation
                                |
                                +--> unavailable/invalid/late -> HOLD model-dependent entry
```

There are no inference retries. Re-evaluating an old state after a timeout creates a
look-ahead/staleness hazard. The same source event timestamp and TTL are carried into the
fallback request.

Neither provider can create an `OrderIntent`. The output remains an immutable advisory
observation and still passes through deterministic policy, hard risk vetoes, journal,
OMS, execution adapter, and reconcile.

Reduce-only maintenance and emergency exits must never depend on either model being
available.

## Laya endpoint

The default is the local self-hosted endpoint:

```
LAYA_SYSTEMONE_ENDPOINT=http://127.0.0.1:8000/v1/systemone
```

The adapter allows plaintext HTTP only for loopback. Any remote Laya endpoint must use
HTTPS so an optional bearer token cannot be leaked in transit.

Optional auth:

```
LAYA_API_KEY=...
```

Deployment provenance:

```
LAYA_BUILD_ID=<immutable image digest or Laya git commit>
```

If `LAYA_BUILD_ID` is omitted, the observation is deliberately labeled
`laya@unpinned:...`. The strategy release gate rejects that evidence with
`unpinned_laya_build`.

The current fallback request pins the typed-decision model id:

```
convaiinnovations/laya-typed-decisions
```

A self-hosted Laya server may report a runtime model plus routing model. Both are retained
in the observation provenance, for example:

```
laya@<build>:laya-rl-agent:typed-decisions
```

## Important calibration rule

Jev and Laya are not treated as statistically interchangeable just because their wire
schemas match. A provider switch can change probability calibration, confidence
distribution, latency, abstention rate, and the set of market states allowed by a
threshold.

Therefore:

- do not reuse a Jev-derived calibration curve as Laya evidence;
- report provider/model provenance for every challenger observation;
- evaluate Laya on the same tape, fees, queue assumptions and latency accounting;
- maintain provider-specific Brier/ECE and p95/p99 latency measurements;
- require independent walk-forward evidence before Laya can contribute to a canary
  promotion decision.

Operational failover is availability redundancy, **not alpha equivalence**.

## Code

- `research/src/pg_tsy/ml/jev_client.py`: strict Jev primary client.
- `research/src/pg_tsy/ml/system_one_failover.py`: strict Laya client plus one-shot
  Jev -> Laya failover.
- `research/tests/test_system_one_failover.py`: schema, TLS/loopback, provenance,
  primary-success, fallback-success and total-outage tests.
- `research/src/pg_tsy/sim/release_gate.py`: blocks unpinned Laya evidence from
  strategy promotion.

This integration does not install Laya as a dependency in the research package. The
boundary is HTTP so the model may be hosted in an isolated GPU service/container and
upgraded independently after calibration.
