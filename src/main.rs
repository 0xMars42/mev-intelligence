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
use mev_intelligence::classify::{BotClass, classify};
use mev_intelligence::cluster::{Observation, cluster_addresses};
use mev_intelligence::config::Config;
use mev_intelligence::db::{Db, SandwichCandidate};
use mev_intelligence::ingest::{TxContext, pending_row};
use mev_intelligence::now_ms;
use mev_intelligence::scan;
use std::time::Instant;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Nombre max de tx validees par passe (borne le cout RPC d'un tick).
const VALIDATE_BATCH: i64 = 50;

/// Plafond de blocs scannés en une passe (borne le coût RPC au 1er tick ou après
/// une longue déconnexion). 120s ≈ 10 blocs, donc 30 garde une large marge.
const MAX_BLOCKS_PER_SCAN: u64 = 30;

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
    info!(
        ws = %cfg.ws_url,
        db = %cfg.database_url,
        stats_secs = cfg.stats_interval.as_secs(),
        "Config chargee"
    );

    let db = Db::connect(&cfg.database_url).await?;
    let already = db.count_pending().await?;
    let outcomes_already = db.count_outcomes().await?;
    info!(
        rows_existantes = already,
        "DB prete (migrations appliquees)"
    );

    let provider = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        ProviderBuilder::new().connect_ws(WsConnect::new(&cfg.ws_url)),
    )
    .await
    .map_err(|_| eyre::eyre!("timeout connexion WebSocket (30s — verifier ETH_WS_URL)"))??;
    info!("WS Ethereum connecte");

    let mut raw: u64 = 0; // tout pending tx recu (avant filtre router)
    let mut total: u64 = 0; // tx ciblant un router whitelist
    let mut inserted: u64 = 0; // nouvelles lignes persistees
    let mut dup: u64 = 0; // doublons ignores (deja vus)
    let mut outcomes_recorded: u64 = 0; // outcomes figes (P3.2)
    let mut last_log = Instant::now();
    let mut validate_tick = tokio::time::interval(cfg.validate_every);
    // Premier analyze après analyze_every (pas immédiatement au démarrage).
    let mut analyze_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + cfg.analyze_every,
        cfg.analyze_every,
    );
    // Scan de blocs pour détection de sandwiches : toutes les 2 minutes.
    // (Réduit à 10s pendant les tests, à remettre à 120s en prod)
    let block_scan_interval = std::time::Duration::from_secs(
        std::env::var("MEV_BLOCK_SCAN_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120),
    );
    let mut block_scan_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + block_scan_interval,
        block_scan_interval,
    );
    // High-water mark : dernier bloc scanné. Permet de couvrir TOUT l'intervalle
    // entre deux scans (un bloc = ~12s, l'intervalle de 120s = ~10 blocs) au lieu
    // de ne regarder que les N derniers et rater les blocs intermédiaires.
    let mut last_scanned_block: u64 = 0;

    // Boucle de reconnexion : si le WS tombe, on re-souscrit au lieu de sortir
    // (un daemon doit survivre aux coupures). Les compteurs au-dessus persistent.
    let mut reconnect_attempt: u32 = 0;
    'reconnect: loop {
        let sub = match provider.subscribe_full_pending_transactions().await {
            Ok(s) => {
                reconnect_attempt = 0;
                s
            }
            Err(e) => {
                reconnect_attempt += 1;
                warn!(err = %e, attempt = reconnect_attempt, "subscribe KO — nouvelle tentative dans 5s");
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
                            reconnect_attempt += 1;
                            warn!(attempt = reconnect_attempt, "stream WS ferme — reconnexion");
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
                        info!(
                            raw,
                            total,
                            inserted,
                            dup,
                            rows_in_db = already + inserted as i64,
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
                            let (label, block, tx_idx) = match outcome {
                                ValidationOutcome::MinedSuccess { block_number, tx_index } => {
                                    ms += 1;
                                    ("MinedSuccess", Some(*block_number as i64), Some(*tx_index as i64))
                                }
                                ValidationOutcome::MinedReverted { block_number, tx_index } => {
                                    mr += 1;
                                    ("MinedReverted", Some(*block_number as i64), Some(*tx_index as i64))
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
                                    ("NotMined", None, None)
                                }
                            };
                            if let Err(e) = db.record_outcome(hash, label, block, tx_idx, now).await {
                                warn!(err = %e, hash = %hash, "record_outcome KO");
                            } else {
                                outcomes_recorded += 1;
                            }
                        }
                        info!(
                            checked = parsed.len(),
                            mined_success = ms,
                            mined_reverted = mr,
                            not_mined = nm,
                            retry_later = retry,
                            outcomes_in_db = outcomes_already + outcomes_recorded as i64,
                            "validation"
                        );
                    }
                }
                _ = analyze_tick.tick() => {
                    info!("analyze auto — debut (cluster + classify + stats)");
                    match run_analyze(&db).await {
                        Ok(()) => info!("analyze auto — termine"),
                        Err(e) => warn!(err = %e, "analyze auto — echec (non bloquant)"),
                    }
                }
                _ = block_scan_tick.tick() => {
                    match scan_block_range(&provider, &db, last_scanned_block, MAX_BLOCKS_PER_SCAN).await {
                        Ok((from, to, n_dex, n_sw)) => {
                            last_scanned_block = to;
                            let n_blocks = to.saturating_sub(from) + 1;
                            if n_sw > 0 {
                                info!(from, to, blocks = n_blocks, dex_swaps = n_dex, sandwiches = n_sw,
                                      "block scan — sandwiches detectes");
                            } else {
                                info!(from, to, blocks = n_blocks, dex_swaps = n_dex, "block scan");
                            }
                        }
                        Err(e) => warn!(err = %e, "block scan KO"),
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

/// Scanne tous les blocs depuis `last_scanned` (exclu) jusqu'au dernier bloc.
/// Couvre l'intégralité de l'intervalle pour ne rater aucun bloc entre deux
/// scans. Borné à `max_blocks` (1er tick / reprise après déconnexion).
/// Retourne `(from, to, dex_swaps_vus, sandwiches_trouvés)`.
async fn scan_block_range<P: Provider>(
    provider: &P,
    db: &Db,
    last_scanned: u64,
    max_blocks: u64,
) -> Result<(u64, u64, usize, usize)> {
    let latest = provider.get_block_number().await?;

    // Premier scan (last_scanned == 0) : on borne à max_blocks en arrière.
    // Sinon : on reprend juste après le dernier bloc scanné, plafonné à max_blocks.
    let from = if last_scanned == 0 {
        latest.saturating_sub(max_blocks.saturating_sub(1))
    } else {
        (last_scanned + 1).max(latest.saturating_sub(max_blocks.saturating_sub(1)))
    };

    if from > latest {
        // Aucun nouveau bloc depuis le dernier scan.
        return Ok((latest, latest, 0, 0));
    }

    let mut total_dex = 0usize;
    let mut total_sw = 0usize;

    for block_num in from..=latest {
        match analyze_block_for_sandwiches(provider, db, block_num).await {
            Ok((dex, sw)) => {
                total_dex += dex;
                total_sw += sw;
            }
            Err(e) => warn!(err = %e, block = block_num, "analyze_block KO"),
        }
    }
    Ok((from, latest, total_dex, total_sw))
}

/// Analyse un bloc via ses receipts (flux de tokens) et persiste les sandwiches.
/// Retourne `(swaps_reconstruits, sandwiches_detectes)`.
async fn analyze_block_for_sandwiches<P: Provider>(
    provider: &P,
    db: &Db,
    block_num: u64,
) -> Result<(usize, usize)> {
    let (n_swaps, sandwiches) = scan::sandwiches_in_block(provider, block_num).await?;
    if sandwiches.is_empty() {
        return Ok((n_swaps, 0));
    }

    let now = now_ms();
    let candidates: Vec<SandwichCandidate> = sandwiches
        .iter()
        .map(|s| scan::to_candidate(s, block_num as i64, now))
        .collect();

    info!(
        block = block_num,
        swaps = n_swaps,
        sandwiches = candidates.len(),
        attacker = %sandwiches[0].attacker,
        token = %sandwiches[0].token,
        pool = %sandwiches[0].pool,
        profit = %scan::fmt_profit(sandwiches[0].gross_profit, sandwiches[0].profit_token.as_deref()),
        "SANDWICH detecte (flux de tokens)"
    );
    db.insert_sandwich_candidates(&candidates).await?;

    Ok((n_swaps, candidates.len()))
}

/// Cluster + classify + stats — même logique que `./mev.sh analyze`, intégrée
/// dans le daemon pour éviter de lancer les binaires séparés périodiquement.
async fn run_analyze(db: &Db) -> Result<()> {
    const WINDOW_MS: i64 = 30_000;
    const MIN_SHARED_TOKENS: usize = 2;

    let now = now_ms();

    // — Clustering —
    let rows = db.swap_observations().await?;
    let n_obs = rows.len();
    let observations: Vec<Observation> = rows
        .into_iter()
        .map(|(from, token_out, seen_at_ms)| Observation {
            from,
            token_out,
            seen_at_ms,
        })
        .collect();
    let clusters = cluster_addresses(&observations, WINDOW_MS, MIN_SHARED_TOKENS);
    db.replace_operators(&clusters, now).await?;
    info!(
        operators = clusters.len(),
        observations = n_obs,
        "analyze: clustering"
    );

    // — Classification + stats —
    let features = db.address_features().await?;
    let classified: Vec<(_, BotClass)> = features
        .into_iter()
        .map(|f| {
            let c = classify(&f);
            (f, c)
        })
        .collect();
    db.replace_bot_classes(&classified, now).await?;
    let stats: Vec<_> = classified.into_iter().map(|(f, _)| f).collect();
    let n_addr = stats.len();
    db.replace_bot_stats(&stats, now).await?;
    info!(addresses = n_addr, "analyze: classification + stats");

    // — Copy-traders + gas wars —
    match db.copy_trader_candidates(10_000, 3).await {
        Ok(pairs) => info!(pairs = pairs.len(), "analyze: copy-traders"),
        Err(e) => warn!(err = %e, "analyze: copy-traders KO"),
    }
    match db.gas_stats_by_class().await {
        Ok(rows) => info!(classes = rows.len(), "analyze: gas stats"),
        Err(e) => warn!(err = %e, "analyze: gas stats KO"),
    }

    Ok(())
}
