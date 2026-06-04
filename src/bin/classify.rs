//! Binaire d'analyse `classify` — taxonomie de bots (P3.4), à la demande.
//!
//! Calcule les features par address depuis la DB, attribue un type
//! ([`mev_intelligence::classify`]), et (re)peuple la table `bot_class`.
//!
//! Usage : `cargo run --bin classify`.

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
use mev_intelligence::classify::{AddressFeatures, BotClass, classify};
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use mev_intelligence::now_ms;
use std::collections::BTreeMap;
use tracing::info;
use tracing_subscriber::EnvFilter;

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
    let classified: Vec<(AddressFeatures, BotClass)> = features
        .into_iter()
        .map(|f| {
            let c = classify(&f);
            (f, c)
        })
        .collect();

    db.replace_bot_classes(&classified, now_ms()).await?;

    // Répartition par classe.
    let mut by_class: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (_, c) in &classified {
        *by_class.entry(c.label()).or_default() += 1;
    }
    info!(addresses = classified.len(), by_class = ?by_class, "classification terminee");

    // Détail des bots non triviaux (ni Generic ni LowActivity).
    for (f, c) in classified
        .iter()
        .filter(|(_, c)| !matches!(c, BotClass::Generic | BotClass::LowActivity))
        .take(20)
    {
        info!(
            address = %f.address,
            class = c.label(),
            tx = f.tx_count,
            distinct_tokens = f.distinct_tokens_out,
            weth_in_ratio = format!("{:.2}", f.weth_in_ratio()),
            avg_gas_gwei = format!("{:.1}", f.avg_max_fee_gwei),
            revert_rate = format!("{:.2}", f.revert_rate()),
            "bot"
        );
    }
    Ok(())
}
