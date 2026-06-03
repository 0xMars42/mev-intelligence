//! Binaire d'analyse `cluster` — étage entity (P3.3), exécuté à la demande.
//!
//! Lit toutes les observations de swap persistées (`pending_tx`), regroupe les
//! addresses d'un même opérateur par co-occurrence (cf. [`mev_intelligence::cluster`]),
//! et (re)peuple la table `operator` / `operator_address`.
//!
//! Usage : `cargo run --bin cluster` (le daemon d'ingestion reste `cargo run`).

#![warn(clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::float_cmp,
    clippy::doc_markdown
)]

use eyre::Result;
use mev_intelligence::cluster::{Observation, cluster_addresses};
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use mev_intelligence::now_ms;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Fenêtre de co-occurrence (ms) : deux swaps sur le même token vus à moins de
/// ça l'un de l'autre comptent comme "ensemble". ~30 s couvre plusieurs blocs.
const WINDOW_MS: i64 = 30_000;
/// Tokens distincts partagés requis pour unir deux addresses. 2 = conservateur :
/// un seul token chaud commun ne suffit pas (faux positifs).
const MIN_SHARED_TOKENS: usize = 2;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if let Err(e) = dotenvy::dotenv()
        && !e.not_found() {
            eprintln!("[warn] .env : {e}");
        }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    let db = Db::connect(&cfg.database_url).await?;

    let rows = db.swap_observations().await?;
    let observations: Vec<Observation> = rows
        .into_iter()
        .map(|(from, token_out, seen_at_ms)| Observation {
            from,
            token_out,
            seen_at_ms,
        })
        .collect();
    info!(
        n_observations = observations.len(),
        window_ms = WINDOW_MS,
        min_shared_tokens = MIN_SHARED_TOKENS,
        "observations chargees, clustering..."
    );

    let clusters = cluster_addresses(&observations, WINDOW_MS, MIN_SHARED_TOKENS);
    let now = now_ms();
    db.replace_operators(&clusters, now).await?;

    info!(
        operators = clusters.len(),
        "clustering termine — table operator (re)peuplee"
    );
    for (i, c) in clusters.iter().enumerate().take(20) {
        info!(
            operator = i + 1,
            size = c.addresses.len(),
            shared_tokens = c.shared_tokens.len(),
            addresses = ?c.addresses,
            tokens = ?c.shared_tokens,
            "operator detecte"
        );
    }
    if clusters.is_empty() {
        info!(
            "aucun cluster multi-address pour l'instant — laisser le daemon \
             d'ingestion accumuler plus de donnees, puis relancer `cluster`"
        );
    }
    Ok(())
}
