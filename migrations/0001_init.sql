-- P3.1 — schema initial : on persiste chaque pending tx ciblant un router DEX
-- whitelist, avec son swap decode. C'est la memoire brute de la plateforme :
-- tout le reste (clustering, classification, P&L) se construit dessus.
--
-- Choix de types pensés portables SQLite <-> Postgres :
--   - adresses / hash / montants U256 : TEXT (decimal pour les montants, 0x.. hex
--     pour adresses/hash). Un U256 ne tient pas dans un INTEGER 64 bits.
--   - timestamps : INTEGER (epoch millis) — simple et trie nativement.

CREATE TABLE IF NOT EXISTS pending_tx (
    hash            TEXT PRIMARY KEY,     -- 0x.. (32 bytes) : dedup naturel
    seen_at_ms      INTEGER NOT NULL,     -- 1er vu dans NOTRE mempool (epoch ms)
    from_addr       TEXT NOT NULL,        -- signer (0x.. 20 bytes)
    to_addr         TEXT NOT NULL,        -- router cible (0x.. 20 bytes)
    router          TEXT NOT NULL,        -- nom lisible (Router::name)
    kind            TEXT NOT NULL,        -- variante DecodedSwap (ExactInput, ...)
    protocol        TEXT,                 -- UniV2 / UniV3 / UniversalRouter / NULL
    selector        TEXT NOT NULL,        -- 4-byte selector (0x........)
    token_in        TEXT,                 -- 0x.. quand decode (sinon NULL)
    token_out       TEXT,
    amount_in_wei   TEXT,                 -- U256 decimal (raw, pas normalise)
    amount_out_min  TEXT,                 -- U256 decimal
    fee_pips        INTEGER,              -- Uni V3 fee tier (NULL sinon)
    max_fee_gwei    REAL NOT NULL,        -- EIP-1559 max_fee_per_gas en gwei
    input_bytes     INTEGER NOT NULL      -- taille du calldata
);

-- Index pensés pour les requetes d'analyse a venir :
--  - by from   : profiler un operateur (toutes ses tx)
--  - token_out : reperer les snipers convergents sur un meme token
--  - router    : repartition par router
--  - seen_at   : fenetres temporelles
CREATE INDEX IF NOT EXISTS idx_pending_tx_from      ON pending_tx (from_addr);
CREATE INDEX IF NOT EXISTS idx_pending_tx_token_out ON pending_tx (token_out);
CREATE INDEX IF NOT EXISTS idx_pending_tx_router    ON pending_tx (router);
CREATE INDEX IF NOT EXISTS idx_pending_tx_seen_at   ON pending_tx (seen_at_ms);
