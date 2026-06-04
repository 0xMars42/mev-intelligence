//! Binaire d'analyse `leaderboard` — analytics & classements (P3.5), à la demande.
//!
//! Calcule les stats par address (volume, activité, succès/revert), (re)peuple
//! `bot_stats`, et imprime les tops.
//!
//! Honnêteté : `weth_volume_eth` est un proxy de TAILLE (ETH injecté), pas un
//! P&L réalisé. Le P&L vrai (matching round-trip + pricing des tokens reçus, dont
//! des memecoins sans prix fiable) est volontairement hors-scope de cette v1.
//!
//! Usage : `cargo run --bin leaderboard`.

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
use mev_intelligence::classify::AddressFeatures;
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use mev_intelligence::now_ms;
use std::fmt::Write as _;
use tracing::info;
use tracing_subscriber::EnvFilter;

const TOP_N: usize = 10;
/// Minimum d'outcomes validés pour qu'un taux (succès/revert) soit fiable.
const MIN_VALIDATED_FOR_RATE: u64 = 3;

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

    let features = db.address_features().await?;
    db.replace_bot_stats(&features, now_ms()).await?;
    info!(addresses = features.len(), "bot_stats (re)peuplee");

    leaderboard(
        "TOP volume (ETH injecte, WETH-in)",
        &features,
        |f| f.weth_volume_eth,
        |f| format!("{:.4} ETH  ({} swaps)", f.weth_volume_eth, f.tx_count),
        None,
    );
    leaderboard(
        "TOP activite (#swaps)",
        &features,
        |f| f.tx_count as f64,
        |f| {
            format!(
                "{} swaps  ({} tokens distincts)",
                f.tx_count, f.distinct_tokens_out
            )
        },
        None,
    );
    leaderboard(
        "TOP success rate (min 3 valides)",
        &features,
        AddressFeatures::success_rate,
        |f| {
            format!(
                "{:.0}%  ({}/{} minees ok)",
                f.success_rate() * 100.0,
                f.mined_success,
                f.validated
            )
        },
        Some(MIN_VALIDATED_FOR_RATE),
    );
    leaderboard(
        "TOP revert rate — races MEV perdues (min 3 valides)",
        &features,
        AddressFeatures::revert_rate,
        |f| {
            format!(
                "{:.0}%  ({}/{} revertees)",
                f.revert_rate() * 100.0,
                f.mined_reverted,
                f.validated
            )
        },
        Some(MIN_VALIDATED_FOR_RATE),
    );

    Ok(())
}

/// Imprime un classement : tri décroissant par `key`, top `TOP_N`, avec un
/// filtre optionnel sur le nombre d'outcomes validés.
fn leaderboard<K, D>(
    title: &str,
    features: &[AddressFeatures],
    key: K,
    display: D,
    min_validated: Option<u64>,
) where
    K: Fn(&AddressFeatures) -> f64,
    D: Fn(&AddressFeatures) -> String,
{
    let mut ranked: Vec<&AddressFeatures> = features
        .iter()
        .filter(|f| min_validated.is_none_or(|m| f.validated >= m))
        .collect();
    ranked.sort_by(|a, b| {
        key(b)
            .partial_cmp(&key(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut body = String::new();
    for (rank, f) in ranked.iter().take(TOP_N).enumerate() {
        let _ = write!(body, "\n  #{:<2} {}  {}", rank + 1, f.address, display(f));
    }
    if body.is_empty() {
        body.push_str("\n  (aucune address eligible)");
    }
    info!("{title}{body}");
}
