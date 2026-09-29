-- Promote exact venue-native fill identity to the durable primary key.
-- Binance/Hyperliquid numeric trade ids remain available as legacy audit data,
-- while IBKR ExecId is stored losslessly in venue_fill_id.
ALTER TABLE execution_fills
    ADD COLUMN IF NOT EXISTS venue_fill_id TEXT;

UPDATE execution_fills
SET venue_fill_id = trade_id::text
WHERE venue_fill_id IS NULL OR venue_fill_id = '';

ALTER TABLE execution_fills
    ALTER COLUMN venue_fill_id SET NOT NULL;

DO $pg_fill_identity$
DECLARE
    pk_def TEXT;
BEGIN
    SELECT pg_get_constraintdef(oid)
    INTO pk_def
    FROM pg_constraint
    WHERE conrelid = 'execution_fills'::regclass
      AND contype = 'p'
    LIMIT 1;

    IF pk_def IS NULL OR pk_def NOT LIKE '%venue_fill_id%' THEN
        IF pk_def IS NOT NULL THEN
            ALTER TABLE execution_fills DROP CONSTRAINT execution_fills_pkey;
        END IF;
        ALTER TABLE execution_fills
            ADD CONSTRAINT execution_fills_pkey
            PRIMARY KEY (account_scope, venue, symbol, venue_fill_id);
    END IF;
END;
$pg_fill_identity$;

DROP INDEX IF EXISTS execution_fills_order_idx;
CREATE INDEX IF NOT EXISTS execution_fills_order_idx
    ON execution_fills (client_order_id, created_at, venue_fill_id);
