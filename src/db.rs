//! Couche de persistance.
//!
//! SQLite via `sqlx` pour l'instant (zéro serveur, embarqué). Le SQL est pensé
//! portable vers Postgres ; les rares bouts SQLite-spécifiques sont **ici** et
//! nulle part ailleurs (aujourd'hui : `INSERT OR IGNORE`, qui devient
//! `ON CONFLICT (hash) DO NOTHING` côté Postgres).
//!
//! Les migrations (`./migrations`) sont embarquées dans le binaire à la
//! compilation et exécutées au démarrage : pas de `sqlx-cli` requis.

use crate::classify::{AddressFeatures, BotClass};
use crate::cluster::Cluster;
use crate::ingest::PendingTxRow;
use eyre::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::collections::HashMap;

/// Tokens "quote" (cotation) Ethereum L1 — WETH/USDC/USDT/DAI. Touchés par
/// presque tout le monde, donc exclus du "token sujet" d'un swap : sinon on
/// clusteriserait / profilerait les bots sur ces tokens communs. Le token sujet
/// = le côté NON-quote (token_out pour un achat, token_in pour une vente) ;
/// NULL si les deux côtés sont des quotes. Corrige le biais où une VENTE a
/// `token_out = WETH` (le même piège que `QUOTE_TOKENS` dans eth-mempool-watcher).
const SUBJECT_TOKEN_SQL: &str = "CASE \
    WHEN token_out IS NOT NULL AND token_out NOT IN ('0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2','0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48','0xdac17f958d2ee523a2206206994597c13d831ec7','0x6b175474e89094c44da98b954eedeac495271d0f') THEN token_out \
    WHEN token_in IS NOT NULL AND token_in NOT IN ('0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2','0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48','0xdac17f958d2ee523a2206206994597c13d831ec7','0x6b175474e89094c44da98b954eedeac495271d0f') THEN token_in \
    ELSE NULL END";

/// Handle de base de données (le `SqlitePool` est `Clone`, partage interne Arc).
#[derive(Clone, Debug)]
pub struct Db {
    pool: SqlitePool,
}

/// Profil consolidé d'une address (stats + classe + opérateur). Sert le serveur
/// MCP et l'API web (d'où `Serialize`).
#[derive(Clone, Debug, serde::Serialize)]
pub struct BotProfile {
    pub address: String,
    pub class: Option<String>,
    pub tx_count: i64,
    pub weth_volume_eth: f64,
    pub distinct_tokens: i64,
    pub validated: i64,
    pub mined_success: i64,
    pub mined_reverted: i64,
    pub not_mined: i64,
    pub success_rate: f64,
    pub revert_rate: f64,
    pub weth_in_ratio: Option<f64>,
    pub avg_max_fee_gwei: Option<f64>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub operator_id: Option<i64>,
    pub operator_size: Option<i64>,
    pub operator_shared_tokens: Option<i64>,
}

impl Db {
    /// Ouvre (ou crée) la base et applique les migrations en attente.
    ///
    /// Accepte les formes `sqlite://fichier.db`, `sqlite:fichier.db` ou un chemin
    /// nu — on extrait le nom de fichier pour éviter les ambiguïtés d'URL SQLite.
    pub async fn connect(database_url: &str) -> Result<Self> {
        let filename = database_url
            .strip_prefix("sqlite://")
            .or_else(|| database_url.strip_prefix("sqlite:"))
            .unwrap_or(database_url);

        let opts = SqliteConnectOptions::new()
            .filename(filename)
            .create_if_missing(true)
            // WAL : meilleures perfs en écriture pour un writer + lecteurs.
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;

        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool })
    }

    /// Insère une ligne `pending_tx`. Idempotent sur `hash` (dédup naturelle si
    /// le même pending tx est revu). Renvoie `true` si une ligne a été insérée,
    /// `false` si c'était un doublon ignoré.
    pub async fn insert_pending(&self, row: &PendingTxRow) -> Result<bool> {
        let res = sqlx::query(
            "INSERT OR IGNORE INTO pending_tx \
             (hash, seen_at_ms, from_addr, to_addr, router, kind, protocol, selector, \
              token_in, token_out, amount_in_wei, amount_out_min, fee_pips, max_fee_gwei, input_bytes) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(row.hash.as_str())
        .bind(row.seen_at_ms)
        .bind(row.from_addr.as_str())
        .bind(row.to_addr.as_str())
        .bind(row.router.as_str())
        .bind(row.kind.as_str())
        .bind(row.protocol.as_deref())
        .bind(row.selector.as_str())
        .bind(row.token_in.as_deref())
        .bind(row.token_out.as_deref())
        .bind(row.amount_in_wei.as_deref())
        .bind(row.amount_out_min.as_deref())
        .bind(row.fee_pips)
        .bind(row.max_fee_gwei)
        .bind(row.input_bytes)
        .execute(&self.pool)
        .await?;

        Ok(res.rows_affected() > 0)
    }

    /// Nombre total de lignes `pending_tx` (pour les stats / smoke test).
    pub async fn count_pending(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_tx")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// Pending tx prêtes à valider : assez vieilles (`seen_at_ms <= max_seen_ms`)
    /// et sans outcome encore figé. Renvoie `(hash, seen_at_ms)`, les plus
    /// anciennes d'abord, bornées à `limit`.
    pub async fn hashes_to_validate(
        &self,
        max_seen_ms: i64,
        limit: i64,
    ) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT p.hash, p.seen_at_ms FROM pending_tx p \
             WHERE p.seen_at_ms <= ? \
               AND NOT EXISTS (SELECT 1 FROM tx_outcome o WHERE o.hash = p.hash) \
             ORDER BY p.seen_at_ms ASC LIMIT ?",
        )
        .bind(max_seen_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Fige l'outcome d'une tx (idempotent sur `hash`).
    pub async fn record_outcome(
        &self,
        hash: &str,
        outcome: &str,
        block_number: Option<i64>,
        checked_at_ms: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO tx_outcome (hash, outcome, block_number, checked_at_ms) \
             VALUES (?,?,?,?)",
        )
        .bind(hash)
        .bind(outcome)
        .bind(block_number)
        .bind(checked_at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Nombre total d'outcomes figés (pour les stats).
    pub async fn count_outcomes(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tx_outcome")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// Observations de swap pour le clustering : `(from, subject_token, seen_at_ms)`,
    /// où `subject_token` est le côté NON-quote du swap (cf. `SUBJECT_TOKEN_SQL`).
    /// On exclut les swaps dont les deux côtés sont des quote tokens (WETH/stables) :
    /// clusteriser sur ces tokens communs créerait de faux liens d'opérateur.
    pub async fn swap_observations(&self) -> Result<Vec<(String, String, i64)>> {
        let sql = format!(
            "SELECT from_addr, subject, seen_at_ms FROM ( \
               SELECT from_addr, seen_at_ms, {SUBJECT_TOKEN_SQL} AS subject FROM pending_tx \
             ) WHERE subject IS NOT NULL ORDER BY seen_at_ms ASC"
        );
        let rows = sqlx::query_as::<_, (String, String, i64)>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Remplace entièrement les opérateurs persistés par le résultat d'un run de
    /// clustering (recalcul complet, atomique via transaction).
    pub async fn replace_operators(&self, clusters: &[Cluster], computed_at_ms: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM operator_address")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM operator")
            .execute(&mut *tx)
            .await?;

        for c in clusters {
            let res = sqlx::query(
                "INSERT INTO operator \
                 (size, shared_token_count, first_seen_ms, last_seen_ms, computed_at_ms) \
                 VALUES (?,?,?,?,?)",
            )
            .bind(c.addresses.len() as i64)
            .bind(c.shared_tokens.len() as i64)
            .bind(c.first_seen_ms)
            .bind(c.last_seen_ms)
            .bind(computed_at_ms)
            .execute(&mut *tx)
            .await?;
            let operator_id = res.last_insert_rowid();

            for address in &c.addresses {
                sqlx::query(
                    "INSERT OR REPLACE INTO operator_address (address, operator_id) VALUES (?,?)",
                )
                .bind(address)
                .bind(operator_id)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    /// Nombre d'opérateurs (clusters) persistés.
    pub async fn count_operators(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM operator")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// Features par address pour la classification (P3.4) : deux agrégats SQL
    /// (`pending_tx` puis jointure `tx_outcome`) recombinés en mémoire.
    pub async fn address_features(&self) -> Result<Vec<AddressFeatures>> {
        // WETH L1 (lowercase, comme on stocke les adresses décodées).
        const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

        // Deux placeholders WETH positionnels (count + volume) -> on bind 2 fois.
        // Le volume est sommé en REAL (proxy ETH-scale ; la perte sub-wei n'a
        // pas d'impact sur un classement), pas en U256 exact.
        // `distinct` compte les tokens SUJETS distincts (côté non-quote), pas les
        // `token_out` bruts : un vendeur multi-tokens n'apparaît plus comme mono-token.
        let sql = format!(
            "SELECT from_addr, COUNT(*), COUNT(DISTINCT {SUBJECT_TOKEN_SQL}), \
                    SUM(CASE WHEN token_in = ? THEN 1 ELSE 0 END), \
                    COUNT(token_in), AVG(max_fee_gwei), \
                    SUM(CASE WHEN token_in = ? THEN CAST(amount_in_wei AS REAL) ELSE 0 END) / 1e18, \
                    MIN(seen_at_ms), MAX(seen_at_ms) \
             FROM pending_tx GROUP BY from_addr"
        );
        let base = sqlx::query_as::<_, (String, i64, i64, i64, i64, f64, f64, i64, i64)>(&sql)
            .bind(WETH)
            .bind(WETH)
            .fetch_all(&self.pool)
            .await?;

        let mut map: HashMap<String, AddressFeatures> = base
            .into_iter()
            .map(
                |(
                    address,
                    tx,
                    distinct,
                    weth_in,
                    token_in_count,
                    avg_gas,
                    volume_eth,
                    first_seen,
                    last_seen,
                )| {
                    (
                        address.clone(),
                        AddressFeatures {
                            address,
                            tx_count: tx as u64,
                            distinct_tokens_out: distinct as u64,
                            weth_in_count: weth_in as u64,
                            token_in_count: token_in_count as u64,
                            avg_max_fee_gwei: avg_gas,
                            validated: 0,
                            mined_success: 0,
                            mined_reverted: 0,
                            not_mined: 0,
                            weth_volume_eth: volume_eth,
                            first_seen_ms: first_seen,
                            last_seen_ms: last_seen,
                        },
                    )
                },
            )
            .collect();

        let outcomes = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT p.from_addr, o.outcome, COUNT(*) \
             FROM pending_tx p JOIN tx_outcome o ON o.hash = p.hash \
             GROUP BY p.from_addr, o.outcome",
        )
        .fetch_all(&self.pool)
        .await?;

        for (address, outcome, count) in outcomes {
            if let Some(f) = map.get_mut(&address) {
                let c = count as u64;
                f.validated += c;
                match outcome.as_str() {
                    "MinedSuccess" => f.mined_success += c,
                    "MinedReverted" => f.mined_reverted += c,
                    _ => f.not_mined += c,
                }
            }
        }

        Ok(map.into_values().collect())
    }

    /// Remplace entièrement la table `bot_class` par une classification fraîche.
    pub async fn replace_bot_classes(
        &self,
        items: &[(AddressFeatures, BotClass)],
        computed_at_ms: i64,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM bot_class")
            .execute(&mut *tx)
            .await?;
        for (f, class) in items {
            sqlx::query(
                "INSERT OR REPLACE INTO bot_class \
                 (address, class, tx_count, distinct_tokens, weth_in_ratio, \
                  avg_max_fee_gwei, revert_rate, classified_at_ms) \
                 VALUES (?,?,?,?,?,?,?,?)",
            )
            .bind(&f.address)
            .bind(class.label())
            .bind(f.tx_count as i64)
            .bind(f.distinct_tokens_out as i64)
            .bind(f.weth_in_ratio())
            .bind(f.avg_max_fee_gwei)
            .bind(f.revert_rate())
            .bind(computed_at_ms)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Remplace entièrement la table `bot_stats` (analytics par address : volume,
    /// activité, taux de succès/revert) — recalcul atomique.
    pub async fn replace_bot_stats(
        &self,
        features: &[AddressFeatures],
        computed_at_ms: i64,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM bot_stats")
            .execute(&mut *tx)
            .await?;
        for f in features {
            sqlx::query(
                "INSERT OR REPLACE INTO bot_stats \
                 (address, tx_count, weth_volume_eth, distinct_tokens, validated, \
                  mined_success, mined_reverted, not_mined, success_rate, revert_rate, \
                  first_seen_ms, last_seen_ms, computed_at_ms) \
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(&f.address)
            .bind(f.tx_count as i64)
            .bind(f.weth_volume_eth)
            .bind(f.distinct_tokens_out as i64)
            .bind(f.validated as i64)
            .bind(f.mined_success as i64)
            .bind(f.mined_reverted as i64)
            .bind(f.not_mined as i64)
            .bind(f.success_rate())
            .bind(f.revert_rate())
            .bind(f.first_seen_ms)
            .bind(f.last_seen_ms)
            .bind(computed_at_ms)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // --- Lecture pour le serveur MCP ---------------------------------------

    /// Répartition des classes de bots (pour le résumé).
    pub async fn bot_class_breakdown(&self) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT class, COUNT(*) FROM bot_class GROUP BY class ORDER BY COUNT(*) DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Nombre d'addresses ayant des stats calculées.
    pub async fn count_bot_stats(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bot_stats")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// Top bots par volume (ETH injecté) ou activité (#swaps), avec leur classe.
    /// Renvoie `(address, class, tx_count, volume_eth, success_rate, revert_rate)`.
    pub async fn top_bots(
        &self,
        by_volume: bool,
        limit: i64,
    ) -> Result<Vec<(String, Option<String>, i64, f64, f64, f64)>> {
        // Colonne de tri choisie par un bool (pas d'input utilisateur dans le SQL).
        let col = if by_volume {
            "s.weth_volume_eth"
        } else {
            "s.tx_count"
        };
        let sql = format!(
            "SELECT s.address, c.class, s.tx_count, s.weth_volume_eth, s.success_rate, s.revert_rate \
             FROM bot_stats s LEFT JOIN bot_class c ON c.address = s.address \
             ORDER BY {col} DESC LIMIT ?"
        );
        let rows = sqlx::query_as::<_, (String, Option<String>, i64, f64, f64, f64)>(&sql)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Profil consolidé d'une address (stats + classe + appartenance opérateur).
    pub async fn bot_profile(&self, address: &str) -> Result<Option<BotProfile>> {
        let stats = sqlx::query_as::<_, (i64, f64, i64, i64, i64, i64, i64, f64, f64, i64, i64)>(
            "SELECT tx_count, weth_volume_eth, distinct_tokens, validated, mined_success, \
                    mined_reverted, not_mined, success_rate, revert_rate, first_seen_ms, last_seen_ms \
             FROM bot_stats WHERE address = ?",
        )
        .bind(address)
        .fetch_optional(&self.pool)
        .await?;
        let Some((tx_count, volume, distinct, validated, ms, mr, nm, sr, rr, first, last)) = stats
        else {
            return Ok(None);
        };

        let (class, weth_in_ratio, avg_max_fee_gwei) =
            match sqlx::query_as::<_, (String, f64, f64)>(
                "SELECT class, weth_in_ratio, avg_max_fee_gwei FROM bot_class WHERE address = ?",
            )
            .bind(address)
            .fetch_optional(&self.pool)
            .await?
            {
                Some((c, w, g)) => (Some(c), Some(w), Some(g)),
                None => (None, None, None),
            };

        let (operator_id, operator_size, operator_shared_tokens) =
            match sqlx::query_as::<_, (i64, i64, i64)>(
                "SELECT o.id, o.size, o.shared_token_count FROM operator_address oa \
                 JOIN operator o ON o.id = oa.operator_id WHERE oa.address = ?",
            )
            .bind(address)
            .fetch_optional(&self.pool)
            .await?
            {
                Some((id, size, shared)) => (Some(id), Some(size), Some(shared)),
                None => (None, None, None),
            };

        Ok(Some(BotProfile {
            address: address.to_string(),
            class,
            tx_count,
            weth_volume_eth: volume,
            distinct_tokens: distinct,
            validated,
            mined_success: ms,
            mined_reverted: mr,
            not_mined: nm,
            success_rate: sr,
            revert_rate: rr,
            weth_in_ratio,
            avg_max_fee_gwei,
            first_seen_ms: first,
            last_seen_ms: last,
            operator_id,
            operator_size,
            operator_shared_tokens,
        }))
    }

    /// Liste des opérateurs (clusters multi-address) avec leurs addresses.
    /// `(id, size, shared_tokens, first_seen_ms, last_seen_ms, addresses)`.
    pub async fn list_operators(&self) -> Result<Vec<(i64, i64, i64, i64, i64, Vec<String>)>> {
        let ops = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            "SELECT id, size, shared_token_count, first_seen_ms, last_seen_ms \
             FROM operator ORDER BY size DESC, shared_token_count DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(ops.len());
        for (id, size, shared, first, last) in ops {
            let addrs = sqlx::query_as::<_, (String,)>(
                "SELECT address FROM operator_address WHERE operator_id = ? ORDER BY address",
            )
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
            out.push((
                id,
                size,
                shared,
                first,
                last,
                addrs.into_iter().map(|(a,)| a).collect(),
            ));
        }
        Ok(out)
    }
}
