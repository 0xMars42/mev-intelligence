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
use alloy::primitives::B256;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use eth_mempool_watcher::decode::decode as decode_swap;
use eth_mempool_watcher::routers::lookup;
use eth_mempool_watcher::validate::{ValidationOutcome, validate_hashes};
use eyre::Result;
use futures_util::StreamExt;
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use mev_intelligence::ingest::{TxContext, pending_row};
use std::time::Instant;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Nombre max de tx validees par passe (borne le cout RPC d'un tick).
const VALIDATE_BATCH: i64 = 50;

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

    let mut raw: u64 = 0; // tout pending tx recu (avant filtre router)
    let mut total: u64 = 0; // tx ciblant un router whitelist
    let mut inserted: u64 = 0; // nouvelles lignes persistees
    let mut dup: u64 = 0; // doublons ignores (deja vus)
    let mut outcomes_recorded: u64 = 0; // outcomes figes (P3.2)
    let mut last_log = Instant::now();
    let mut validate_tick = tokio::time::interval(cfg.validate_every);

    // Boucle de reconnexion : si le WS tombe, on re-souscrit au lieu de sortir
    // (un daemon doit survivre aux coupures). Les compteurs au-dessus persistent.
    'reconnect: loop {
        let sub = match provider.subscribe_full_pending_transactions().await {
            Ok(s) => s,
            Err(e) => {
                warn!(err = %e, "subscribe KO — nouvelle tentative dans 5s");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue 'reconnect;
            }
        };
        let mut stream = sub.into_stream();
        info!("Subscription `newPendingTransactions` (full bodies) active — Ctrl+C pour arreter");

        loop {
            tokio::select! {
                    maybe_tx = stream.next() => {
                        let Some(tx) = maybe_tx else {
                            warn!("stream pending ferme (WS coupe ?) — reconnexion");
                            break;
                        };
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
                _ = validate_tick.tick() => {
                    // Passe de validation : pour les pending tx assez vieilles et pas
                    // encore figees, on lit le receipt et on enregistre l'outcome.
                    let now = now_ms();
                    let max_seen = now - cfg.validate_min_age.as_millis() as i64;
                    let candidates = db
                        .hashes_to_validate(max_seen, VALIDATE_BATCH)
                        .await
                        .unwrap_or_default();
                    let parsed: Vec<(String, i64, B256)> = candidates
                        .into_iter()
                        .filter_map(|(h, s)| h.parse::<B256>().ok().map(|b| (h, s, b)))
                        .collect();
                    if !parsed.is_empty() {
                        let hashes: Vec<B256> = parsed.iter().map(|(_, _, b)| *b).collect();
                        let outcomes = validate_hashes(&provider, &hashes).await;
                        let drop_before = now - cfg.validate_max_age.as_millis() as i64;
                        let (mut ms, mut mr, mut nm, mut retry) = (0u32, 0u32, 0u32, 0u32);
                        for ((hash, seen_at, _), (_, outcome)) in parsed.iter().zip(&outcomes) {
                            let (label, block) = match outcome {
                                ValidationOutcome::MinedSuccess { block_number } => {
                                    ms += 1;
                                    ("MinedSuccess", Some(*block_number as i64))
                                }
                                ValidationOutcome::MinedReverted { block_number } => {
                                    mr += 1;
                                    ("MinedReverted", Some(*block_number as i64))
                                }
                                ValidationOutcome::NotMined => {
                                    // Pas de receipt : on ne fige NotMined que si la tx est
                                    // vraiment vieille ; sinon elle peut encore etre minee,
                                    // on la re-testera au prochain tick.
                                    if *seen_at > drop_before {
                                        retry += 1;
                                        continue;
                                    }
                                    nm += 1;
                                    ("NotMined", None)
                                }
                            };
                            if let Err(e) = db.record_outcome(hash, label, block, now).await {
                                warn!(err = %e, hash = %hash, "record_outcome KO");
                            } else {
                                outcomes_recorded += 1;
                            }
                        }
                        let in_db = db.count_outcomes().await.unwrap_or(-1);
                        info!(
                            checked = parsed.len(),
                            mined_success = ms,
                            mined_reverted = mr,
                            not_mined = nm,
                            retry_later = retry,
                            outcomes_in_db = in_db,
                            "validation"
                        );
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl+C recu — arret propre");
                    break 'reconnect;
                }
            }
        }
        // Stream interne tombe (WS coupe) : petit backoff avant de re-souscrire.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    let in_db = db.count_pending().await.unwrap_or(-1);
    let outcomes_in_db = db.count_outcomes().await.unwrap_or(-1);
    info!(
        raw,
        total,
        inserted,
        dup,
        rows_in_db = in_db,
        outcomes_recorded,
        outcomes_in_db,
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
