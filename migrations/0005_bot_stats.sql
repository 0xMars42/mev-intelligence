-- P3.5 — analytics par address : volume, activité, taux de succès/revert.
--
-- Recalculé à chaque run du binaire `leaderboard`. Sert de base aux classements
-- ("top bots par volume / par success rate"). NB : `weth_volume_eth` est un
-- PROXY de taille (somme des ETH injectés sur les swaps WETH-in) — PAS un P&L
-- réalisé (qui exigerait le matching round-trip + le pricing des tokens reçus).

CREATE TABLE IF NOT EXISTS bot_stats (
    address          TEXT PRIMARY KEY,
    tx_count         INTEGER NOT NULL,
    weth_volume_eth  REAL NOT NULL,   -- ETH injecté (proxy volume), pas un P&L
    distinct_tokens  INTEGER NOT NULL,
    validated        INTEGER NOT NULL,
    mined_success    INTEGER NOT NULL,
    mined_reverted   INTEGER NOT NULL,
    not_mined        INTEGER NOT NULL,
    success_rate     REAL NOT NULL,
    revert_rate      REAL NOT NULL,
    first_seen_ms    INTEGER NOT NULL,
    last_seen_ms     INTEGER NOT NULL,
    computed_at_ms   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_bot_stats_volume  ON bot_stats (weth_volume_eth);
CREATE INDEX IF NOT EXISTS idx_bot_stats_txcount ON bot_stats (tx_count);
