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

/// Handle de base de données (le `SqlitePool` est `Clone`, partage interne Arc).
#[derive(Clone, Debug)]
pub struct Db {
    pool: SqlitePool,
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

    /// Observations de swap pour le clustering : `(from, token_out, seen_at_ms)`
    /// de chaque pending tx ayant un `token_out` décodé, triées par temps.
    pub async fn swap_observations(&self) -> Result<Vec<(String, String, i64)>> {
        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT from_addr, token_out, seen_at_ms FROM pending_tx \
             WHERE token_out IS NOT NULL ORDER BY seen_at_ms ASC",
        )
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

        let base = sqlx::query_as::<_, (String, i64, i64, i64, i64, f64)>(
            "SELECT from_addr, COUNT(*), COUNT(DISTINCT token_out), \
                    SUM(CASE WHEN token_in = ?1 THEN 1 ELSE 0 END), \
                    COUNT(token_in), AVG(max_fee_gwei) \
             FROM pending_tx GROUP BY from_addr",
        )
        .bind(WETH)
        .fetch_all(&self.pool)
        .await?;

        let mut map: HashMap<String, AddressFeatures> = base
            .into_iter()
            .map(
                |(address, tx, distinct, weth_in, token_in_count, avg_gas)| {
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
}
