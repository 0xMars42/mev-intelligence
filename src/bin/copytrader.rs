//! Binaire `copytrader` — détection de copy-traders et analyse des guerres de gas (P3+).
//!
//! Copy-trader : address B qui envoie un swap sur le même token qu'address A
//! systématiquement dans une courte fenêtre (~10 s). Signal : même token sujet,
//! même direction temporelle, N fois minimum.
//!
//! Gas wars : distribution de `max_fee_gwei` par classe de bot — révèle qui
//! bid le plus agressivement pour la position dans le bloc.
//!
//! Usage : `cargo run --bin copytrader` ou `./mev.sh analyze`.

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
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Fenêtre de suivi : B doit envoyer sa tx dans les N ms après A pour compter.
/// 60s = ~5 blocs. Les paires du même opérateur sont filtrées (rotation de wallets ≠ copy-trade).
const WINDOW_MS: i64 = 60_000;
/// Nombre minimum de copies pour considérer un lien copy-trader significatif.
const MIN_COPIES: i64 = 3;
/// Seuil gas (gwei) pour le classement des snipers/bids extrêmes.
const HIGH_GAS_GWEI: f64 = 50.0;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if let Err(e) = dotenvy::dotenv()
        && !e.not_found()
    {
        eprintln!("[warn] .env : {e}");
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    let db = Db::connect(&cfg.database_url).await?;

    // ── Copy-traders ────────────────────────────────────────────────────────
    let candidates = db.copy_trader_candidates(WINDOW_MS, MIN_COPIES).await?;

    if candidates.is_empty() {
        info!(
            "copy-traders : aucun candidat (fenetre={}s, min_copies={})",
            WINDOW_MS / 1000,
            MIN_COPIES
        );
    } else {
        info!(
            count = candidates.len(),
            "COPY-TRADERS — paires detectees (fenetre {}s, min {} copies)",
            WINDOW_MS / 1000,
            MIN_COPIES
        );
        for (i, (leader, follower, tokens, copies, avg_delay)) in candidates.iter().enumerate() {
            info!(
                rank = i + 1,
                leader = %leader,
                follower = %follower,
                tokens_copies = tokens,
                nb_copies = copies,
                avg_delay_ms = avg_delay,
                avg_delay_s = format!("{:.1}s", *avg_delay as f64 / 1000.0),
                "copy-trader candidat"
            );
        }
    }

    // ── Gas wars — distribution par classe ──────────────────────────────────
    let gas_stats = db.gas_stats_by_class().await?;
    info!("GAS WARS — distribution max_fee_gwei par classe :");
    for (class, tx_count, avg, min_g, max_g, addr_count) in &gas_stats {
        info!(
            class = %class,
            addrs = addr_count,
            txs = tx_count,
            avg_gwei = avg,
            min_gwei = min_g,
            max_gwei = max_g,
            ratio_max_avg = format!("{:.1}x", max_g / avg.max(0.001)),
            "gas stats"
        );
    }

    // ── Snipers — top gas bidders ────────────────────────────────────────────
    let snipers = db.top_gas_bidders(HIGH_GAS_GWEI, 10).await?;
    if snipers.is_empty() {
        info!("snipers : aucun bid > {HIGH_GAS_GWEI} gwei");
    } else {
        info!("TOP SNIPERS (peak gas > {HIGH_GAS_GWEI} gwei) :");
        for (i, (addr, peak, nb_tx, class)) in snipers.iter().enumerate() {
            info!(
                rank = i + 1,
                address = %addr,
                peak_gwei = peak,
                nb_tx_high_gas = nb_tx,
                class = class.as_deref().unwrap_or("?"),
                "sniper"
            );
        }
    }

    Ok(())
}
