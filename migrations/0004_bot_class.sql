-- P3.4 — taxonomie : un type de bot par address, depuis son fingerprint.
--
-- Recalculé à chaque run du binaire `classify`. On stocke aussi les features
-- clés qui ont mené à la classe (transparence : la classe est explicable).

CREATE TABLE IF NOT EXISTS bot_class (
    address          TEXT PRIMARY KEY,
    class            TEXT NOT NULL,    -- Sniper | SingleTarget | Racer | Generic | LowActivity
    tx_count         INTEGER NOT NULL,
    distinct_tokens  INTEGER NOT NULL,
    weth_in_ratio    REAL NOT NULL,
    avg_max_fee_gwei REAL NOT NULL,
    revert_rate      REAL NOT NULL,
    classified_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_bot_class_class ON bot_class (class);
