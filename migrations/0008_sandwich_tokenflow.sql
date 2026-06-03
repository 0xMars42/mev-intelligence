-- Migration 0008 : enrichit sandwich_candidate pour la détection par flux de tokens.
--
-- La détection passe du décodage calldata (limité aux routers publics) à l'analyse
-- des Transfer logs (capte les sandwicheurs via contrats custom). On ajoute :
--  - pool          : le pool sur lequel le sandwich a eu lieu (adresse)
--  - front_amount  : quantité de token sujet achetée au frontrun (texte, u128)
--  - back_amount   : quantité revendue au backrun (texte, u128)
-- Montants en TEXT car un u128 peut dépasser la capacité d'un INTEGER SQLite (i64).

ALTER TABLE sandwich_candidate ADD COLUMN pool TEXT;
ALTER TABLE sandwich_candidate ADD COLUMN front_amount TEXT;
ALTER TABLE sandwich_candidate ADD COLUMN back_amount TEXT;
