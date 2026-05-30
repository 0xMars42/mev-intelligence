-- P3.3 — entity layer : regroupe les addresses d'un meme operateur.
--
-- Un "operator" = un cluster d'addresses qui agissent de concert (heuristique
-- de co-occurrence : memes tokens cibles, memes fenetres temporelles, sur
-- plusieurs tokens distincts). Recalcule a chaque run du binaire `cluster`.
--
-- On ne persiste QUE les clusters de taille >= 2 (un wallet seul n'est pas un
-- "regroupement"). `shared_token_count` = nb de tokens "preuve" qui lient le
-- cluster — plus il est haut, plus le regroupement est solide.

CREATE TABLE IF NOT EXISTS operator (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    size               INTEGER NOT NULL,  -- nb d'addresses dans le cluster
    shared_token_count INTEGER NOT NULL,  -- nb de tokens "preuve" du lien
    first_seen_ms      INTEGER NOT NULL,  -- 1re activite observee du cluster
    last_seen_ms       INTEGER NOT NULL,  -- derniere activite observee
    computed_at_ms     INTEGER NOT NULL   -- quand ce cluster a ete calcule
);

CREATE TABLE IF NOT EXISTS operator_address (
    address     TEXT PRIMARY KEY,         -- une address appartient a un operateur
    operator_id INTEGER NOT NULL REFERENCES operator (id)
);

CREATE INDEX IF NOT EXISTS idx_operator_address_op ON operator_address (operator_id);
