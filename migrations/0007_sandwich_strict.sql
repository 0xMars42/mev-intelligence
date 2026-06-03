-- Migration 0007 : durcit la détection de sandwich.
--
-- 1) Purge les candidats calculés par l'ancienne logique (positionnelle seule,
--    sans vérif du sens achat/vente). 84% avaient un token NULL = faux positifs.
--    Les blocs ne sont pas re-scannés (high-water mark), donc on repart propre.
-- 2) Contrainte UNIQUE réelle : sans elle, `INSERT OR IGNORE` ne dédupliquait
--    rien (bug : doublons possibles au redémarrage du service quand le 1er scan
--    reprend les 30 derniers blocs).

DELETE FROM sandwich_candidate;

CREATE UNIQUE INDEX IF NOT EXISTS idx_sandwich_unique
    ON sandwich_candidate(frontrun_hash, victim_hash, backrun_hash);
