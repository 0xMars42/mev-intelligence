//! Couche de persistance.
//!
//! SQLite via `sqlx` pour l'instant (zéro serveur, embarqué). Le SQL est pensé
//! portable vers Postgres ; les rares bouts SQLite-spécifiques sont **ici** et
//! nulle part ailleurs (aujourd'hui : `INSERT OR IGNORE`, qui devient
//! `ON CONFLICT (hash) DO NOTHING` côté Postgres).
//!
//! Les migrations (`./migrations`) sont embarquées dans le binaire à la
//! compilation et exécutées au démarrage : pas de `sqlx-cli` requis.

use crate::ingest::PendingTxRow;
use eyre::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

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
}
