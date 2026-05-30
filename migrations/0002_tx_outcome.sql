-- P3.2 — outcome reel de chaque pending tx, via son receipt.
--
-- On separe l'ingestion (ecrit `pending_tx`) de la validation (ecrit
-- `tx_outcome`) : 1 ligne par hash, jointe sur `hash`. Le dataset "swap +
-- outcome" = `pending_tx JOIN tx_outcome USING (hash)`.
--
-- outcome :
--   MinedSuccess  : incluse, status=true  (le swap a probablement reussi)
--   MinedReverted : incluse, status=false (gas paye pour rien — race MEV perdue)
--   NotMined      : aucun receipt apres la fenetre -> droppee / remplacee

CREATE TABLE IF NOT EXISTS tx_outcome (
    hash          TEXT PRIMARY KEY,   -- reference logique vers pending_tx.hash
    outcome       TEXT NOT NULL,      -- MinedSuccess | MinedReverted | NotMined
    block_number  INTEGER,            -- bloc d'inclusion (NULL si NotMined)
    checked_at_ms INTEGER NOT NULL    -- quand on a fige l'outcome (epoch ms)
);

CREATE INDEX IF NOT EXISTS idx_tx_outcome_outcome ON tx_outcome (outcome);
