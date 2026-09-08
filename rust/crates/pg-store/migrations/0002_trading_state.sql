CREATE TABLE IF NOT EXISTS order_records (
    client_order_id TEXT PRIMARY KEY,
    venue TEXT NOT NULL,
    asset TEXT NOT NULL,
    owner_strategy_id TEXT NOT NULL,
    state JSONB NOT NULL,
    fencing_token BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS order_records_scope_idx
    ON order_records (venue, asset, updated_at DESC);

CREATE TABLE IF NOT EXISTS position_ownership (
    venue TEXT NOT NULL,
    asset TEXT NOT NULL,
    position_state JSONB NOT NULL,
    observed_quantity TEXT NOT NULL,
    fencing_token BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (venue, asset)
);

CREATE TABLE IF NOT EXISTS reconcile_runs (
    reconcile_id UUID PRIMARY KEY,
    venue TEXT NOT NULL,
    report JSONB NOT NULL,
    fencing_token BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS reconcile_runs_venue_created_idx
    ON reconcile_runs (venue, created_at DESC);
