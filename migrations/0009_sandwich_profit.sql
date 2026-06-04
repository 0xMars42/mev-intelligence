-- Migration 0009 : profit brut estimé par sandwich.
--
-- gross_profit : (quote reçu au backrun − quote dépensé au frontrun), en unités
--   du quote token (wei pour WETH). HORS gas. Texte car i128 dépasse i64.
-- profit_token : le quote token de référence (WETH/USDC/...). NULL si les deux
--   pattes n'utilisent pas le même quote (profit non calculable de façon fiable).

ALTER TABLE sandwich_candidate ADD COLUMN gross_profit TEXT;
ALTER TABLE sandwich_candidate ADD COLUMN profit_token TEXT;
