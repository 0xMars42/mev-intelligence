-- Migration 0006 : analyse intra-bloc pour détection de sandwiches.
--
-- tx_index : position d'une tx dans son bloc (0 = première tx). Ajouté à
-- tx_outcome car on l'obtient gratuitement depuis le receipt (transaction_index).
-- Essentiel pour ordonner les txs dans un bloc et détecter les patterns MEV.

ALTER TABLE tx_outcome ADD COLUMN tx_index INTEGER;

-- Candidats sandwich détectés dans un bloc.
-- Un sandwich = [frontrun du bot] → [victim (tx cible)] → [backrun du bot]
-- sur le même token, dans le même bloc, positions frontrun < victim < backrun.
CREATE TABLE IF NOT EXISTS sandwich_candidate (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    block_number    INTEGER NOT NULL,
    frontrun_hash   TEXT    NOT NULL,
    victim_hash     TEXT    NOT NULL,
    backrun_hash    TEXT    NOT NULL,
    attacker_addr   TEXT    NOT NULL,
    target_token    TEXT,                  -- token sujet (NULL si non décodé)
    frontrun_idx    INTEGER NOT NULL,      -- position dans le bloc
    victim_idx      INTEGER NOT NULL,
    backrun_idx     INTEGER NOT NULL,
    detected_at_ms  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sandwich_block    ON sandwich_candidate(block_number);
CREATE INDEX IF NOT EXISTS idx_sandwich_attacker ON sandwich_candidate(attacker_addr);
