//! Binaire `scan-range` — scanne une plage de blocs pour détecter les sandwiches.
//!
//! Sert à deux choses :
//!  - **Tester** la détection sur un grand échantillon (taux de détection, qualité)
//!  - **Backfill** : rattraper l'historique que le daemon n'a pas couvert
//!
//! Usage :
//!   scan-range <from> <to>     # scanne les blocs [from, to]
//!   scan-range <last_n>        # scanne les <last_n> derniers blocs
//!   scan-range                 # défaut : 50 derniers blocs
//!
//! Les sandwiches détectés sont insérés en DB (idempotent via l'index unique).

#![warn(clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use eyre::Result;
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use mev_intelligence::{now_ms, scan};
use std::collections::HashMap;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// WETH (quote de référence pour agréger les profits).
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

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
    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(&cfg.ws_url))
        .await?;
    let latest = provider.get_block_number().await?;

    // Parse les arguments : (from, to).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (from, to) = match args.as_slice() {
        [] => (latest.saturating_sub(49), latest),
        [n] => {
            let n: u64 = n
                .parse()
                .map_err(|_| eyre::eyre!("argument invalide: {n}"))?;
            (latest.saturating_sub(n.saturating_sub(1)), latest)
        }
        [f, t, ..] => (
            f.parse().map_err(|_| eyre::eyre!("from invalide: {f}"))?,
            t.parse().map_err(|_| eyre::eyre!("to invalide: {t}"))?,
        ),
    };
    if from > to {
        eyre::bail!("from ({from}) > to ({to})");
    }

    info!(
        from,
        to,
        blocks = to - from + 1,
        latest,
        "scan-range — debut"
    );

    let mut total_swaps = 0usize;
    let mut total_sandwiches = 0usize;
    // Par attaquant : (nb sandwiches, profit WETH POSITIF cumulé en wei).
    // On ne somme que les profits positifs : un net négatif est soit un faux
    // positif du pattern, soit un artefact (profit routé via un contrat tiers
    // hors {from,to} → entrée WETH non vue). Le total est donc un MINORANT.
    let mut by_attacker: HashMap<String, (usize, i128)> = HashMap::new();
    let mut blocks_scanned = 0u64;
    let mut errors = 0u64;
    let mut nb_profitable = 0usize; // sandwiches à profit WETH net > 0

    for block_num in from..=to {
        match scan::sandwiches_in_block(&provider, block_num).await {
            Ok((n_swaps, sandwiches)) => {
                blocks_scanned += 1;
                total_swaps += n_swaps;
                if !sandwiches.is_empty() {
                    let now = now_ms();
                    let candidates: Vec<_> = sandwiches
                        .iter()
                        .map(|s| scan::to_candidate(s, block_num as i64, now))
                        .collect();
                    db.insert_sandwich_candidates(&candidates).await?;
                    total_sandwiches += sandwiches.len();
                    for s in &sandwiches {
                        let entry = by_attacker.entry(s.attacker.clone()).or_default();
                        entry.0 += 1;
                        if s.profit_token.as_deref() == Some(WETH) && s.gross_profit > 0 {
                            entry.1 += s.gross_profit;
                            nb_profitable += 1;
                        }
                        info!(
                            block = block_num,
                            attacker = %s.attacker,
                            token = %s.token,
                            pos = format!("{}<{}<{}", s.frontrun_idx, s.victim_idx, s.backrun_idx),
                            profit = %scan::fmt_profit(s.gross_profit, s.profit_token.as_deref()),
                            "sandwich"
                        );
                    }
                }
            }
            Err(e) => {
                errors += 1;
                warn!(err = %e, block = block_num, "scan bloc KO");
            }
        }
    }

    // Top attaquants : tri par profit WETH positif cumulé décroissant.
    let mut ranking: Vec<(String, (usize, i128))> = by_attacker.into_iter().collect();
    ranking.sort_by_key(|x| std::cmp::Reverse(x.1.1));
    let total_profit_weth: i128 = ranking.iter().map(|x| x.1.1).sum();

    info!(
        blocks_scanned,
        errors,
        swaps_reconstruits = total_swaps,
        sandwiches_detectes = total_sandwiches,
        sandwiches_rentables = nb_profitable,
        swaps_par_bloc = format!("{:.1}", total_swaps as f64 / blocks_scanned.max(1) as f64),
        profit_weth_extrait = format!(
            "{:.4} (minorant, positifs only)",
            total_profit_weth as f64 / 1e18
        ),
        "scan-range — bilan"
    );
    for (i, (attacker, (n, profit))) in ranking.iter().take(10).enumerate() {
        info!(
            rank = i + 1,
            attacker = %attacker,
            sandwiches = n,
            profit_weth = format!("{:.4}", *profit as f64 / 1e18),
            "top attaquant"
        );
    }

    Ok(())
}
