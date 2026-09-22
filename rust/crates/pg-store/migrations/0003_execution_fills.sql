-- One durable trade ID per account, venue and instrument. Never infer a fill
-- from an order's cumulative executed quantity or overwrite conflicting trades.
CREATE TABLE IF NOT EXISTS execution_fills (
    account_scope TEXT NOT NULL CHECK (length(account_scope) BETWEEN 1 AND 128),
    venue TEXT NOT NULL,
    symbol TEXT NOT NULL,
    trade_id BIGINT NOT NULL CHECK (trade_id >= 0),
    client_order_id TEXT NOT NULL REFERENCES order_records(client_order_id),
    fill_data JSONB NOT NULL,
    fencing_token BIGINT NOT NULL CHECK (fencing_token > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_scope, venue, symbol, trade_id)
);

CREATE INDEX IF NOT EXISTS execution_fills_order_idx
    ON execution_fills (client_order_id, trade_id);

-- save_order_record uses INSERT ... ON CONFLICT DO UPDATE. A repeated dispatch
-- previously replaced a durable PendingSubmit/Unknown/Open/Filled order with
-- a fresh Created record before submitting the same client ID again. Reject
-- *every* conflicting Created write, including one against an existing Created
-- record left by a crash. The initial INSERT does not fire an UPDATE trigger.
-- An existing deployment runs raw_sql migrations at startup; creating the
-- trigger conditionally keeps repeated migrations idempotent.
CREATE OR REPLACE FUNCTION pg_reject_duplicate_order_intent()
RETURNS trigger LANGUAGE plpgsql AS $pg_order_identity$
BEGIN
    IF NEW.state->>'state' = 'Created' THEN
        RAISE EXCEPTION 'duplicate durable order intent: %', NEW.client_order_id
            USING ERRCODE = '23505';
    END IF;
    RETURN NEW;
END;
$pg_order_identity$;

DO $pg_install_guard$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_trigger
        WHERE tgrelid = 'order_records'::regclass
          AND tgname = 'pg_order_intent_once'
          AND NOT tgisinternal
    ) THEN
        CREATE TRIGGER pg_order_intent_once
        BEFORE UPDATE OF state ON order_records
        FOR EACH ROW EXECUTE FUNCTION pg_reject_duplicate_order_intent();
    END IF;
END;
$pg_install_guard$;
