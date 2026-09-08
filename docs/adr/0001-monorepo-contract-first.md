# ADR-0001: Monorepo and contract-first research/live split

Status: Accepted

## Decision

Use one monorepo. Keep Python research and Rust live execution in separate package/workspace trees. Cross the boundary only through versioned contracts and immutable model/data artifacts.

## Consequences

Positive: smaller cognitive load, atomic contract changes, easier AI-agent context, one CI surface.

Negative: repo can grow large; discipline is required to prevent Python live-execution shortcuts and Rust research coupling.
