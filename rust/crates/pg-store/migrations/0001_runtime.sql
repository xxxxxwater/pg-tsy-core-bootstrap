CREATE TABLE IF NOT EXISTS runtime_leases (
    lease_key TEXT PRIMARY KEY,
    holder_id TEXT NOT NULL,
    fencing_token BIGINT NOT NULL CHECK (fencing_token > 0),
    lease_expires_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS event_journal (
    sequence BIGSERIAL PRIMARY KEY,
    event_id UUID NOT NULL UNIQUE,
    stream_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    fencing_token BIGINT NOT NULL CHECK (fencing_token > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS event_journal_stream_sequence_idx
    ON event_journal (stream_id, sequence);

CREATE TABLE IF NOT EXISTS checkpoints (
    stream_id TEXT PRIMARY KEY,
    journal_sequence BIGINT NOT NULL CHECK (journal_sequence >= 0),
    state JSONB NOT NULL,
    fencing_token BIGINT NOT NULL CHECK (fencing_token > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS command_audit (
    id BIGSERIAL PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    actor_user_id BIGINT NOT NULL,
    chat_id BIGINT NOT NULL,
    command JSONB NOT NULL,
    outcome TEXT NOT NULL,
    fencing_token BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS command_audit_created_at_idx
    ON command_audit (created_at DESC);
