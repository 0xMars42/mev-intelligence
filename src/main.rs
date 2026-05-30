//! mev-intelligence — binaire d'ingestion (P3.1).
//!
//! Stream les pending tx bodies Ethereum (WebSocket), filtre sur la whitelist
//! de routers DEX (réutilisée de P2), décode le swap, et **persiste** chaque hit
//! dans SQLite. C'est la couche qui manquait à P2 : la mémoire.
//!
//! Boucle : `subscribe_full_pending_transactions` -> lookup router -> decode ->
//! `pending_row` (pur) -> `db.insert_pending`. Stats cumulées périodiques.

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
    // doc_markdown : backticks exiges autour de SQLite/WebSocket... = bruit sur la prose.
    clippy::doc_markdown,
    // main() est une boucle d'orchestration async ; la découper nuirait à la lisibilité.
    clippy::too_many_lines
)]

use alloy::consensus::Transaction;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use eth_mempool_watcher::decode::decode as decode_swap;
use eth_mempool_watcher::routers::lookup;
use eyre::Result;
use futures_util::StreamExt;
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use mev_intelligence::ingest::{TxContext, pending_row};
use std::time::Instant;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    info!(
        ws = %cfg.ws_url,
        db = %cfg.database_url,
        stats_secs = cfg.stats_interval.as_secs(),
        "Config chargee"
    );

    let db = Db::connect(&cfg.database_url).await?;
    let already = db.count_pending().await?;
    info!(
        rows_existantes = already,
        "DB prete (migrations appliquees)"
    );

    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(&cfg.ws_url))
        .await?;
    info!("WS Ethereum connecte");

    let sub = provider.subscribe_full_pending_transactions().await?;
    let mut stream = sub.into_stream();
    info!("Subscription `newPendingTransactions` (full bodies) active — Ctrl+C pour arreter");

    let mut raw: u64 = 0; // tout pending tx recu (avant filtre router)
    let mut total: u64 = 0; // tx ciblant un router whitelist
    let mut inserted: u64 = 0; // nouvelles lignes persistees
    let mut dup: u64 = 0; // doublons ignores (deja vus)
    let mut last_log = Instant::now();

    loop {
        tokio::select! {
            maybe_tx = stream.next() => {
                let Some(tx) = maybe_tx else { break };
                raw += 1;

                // Filtre : seulement les tx vers un router DEX whitelist.
                let Some(to) = tx.inner.to() else { continue };
                let Some(router) = lookup(to) else { continue };
                total += 1;

                let input = tx.inner.input();
                let decoded = decode_swap(router, input);
                let ctx = TxContext {
                    hash: *tx.inner.hash(),
                    from: tx.inner.signer(),
                    to,
                    value: tx.inner.value(),
                    max_fee_per_gas: tx.inner.max_fee_per_gas(),
                    input: input.to_vec(),
                };
                let row = pending_row(router, &decoded, &ctx, now_ms());

                match db.insert_pending(&row).await {
                    Ok(true) => {
                        inserted += 1;
                        info!(
                            router = row.router,
                            kind = row.kind,
                            token_out = row.token_out.as_deref().unwrap_or("-"),
                            from = row.from_addr,
                            hash = row.hash,
                            "pending persiste"
                        );
                    }
                    Ok(false) => dup += 1,
                    Err(e) => warn!(err = %e, hash = row.hash, "insert KO"),
                }

                if last_log.elapsed() >= cfg.stats_interval {
                    let in_db = db.count_pending().await.unwrap_or(-1);
                    info!(
                        raw,
                        total,
                        inserted,
                        dup,
                        rows_in_db = in_db,
                        "stats ingestion"
                    );
                    last_log = Instant::now();
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C recu — arret propre");
                break;
            }
        }
    }

    let in_db = db.count_pending().await.unwrap_or(-1);
    info!(
        raw,
        total,
        inserted,
        dup,
        rows_in_db = in_db,
        "arret — bilan"
    );
    Ok(())
}

/// Epoch millis courant (timestamp de 1re observation d'un pending tx).
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
