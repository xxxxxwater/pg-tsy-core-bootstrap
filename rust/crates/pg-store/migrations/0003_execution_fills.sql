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
