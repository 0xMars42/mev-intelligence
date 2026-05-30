//! mev-intelligence — racine de la librairie.
//!
//! Plateforme d'intelligence MEV : ingère le mempool public Ethereum, persiste
//! chaque swap pending décodé, et (phases suivantes) clusterise les opérateurs,
//! classifie leurs stratégies et estime leur P&L.
//!
//! Ce crate **consomme** [`eth_mempool_watcher`] (projet P2) comme librairie
//! d'ingestion : `routers` + `decode` y sont déjà validés en live. P3 ajoute la
//! couche qui manquait à P2 — la **mémoire** (persistance) et l'**analyse**.
//!
//! Modules :
//! - [`config`] : configuration runtime (URLs, DB) depuis l'environnement.
//! - [`db`]     : couche de persistance (SQLite via sqlx, portable Postgres).
//! - [`ingest`] : mapping `DecodedSwap` -> ligne `pending_tx` (logique pure).

#![warn(clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    // Math d'affichage : casts f64 volontaires (gwei, montants lisibles).
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::float_cmp,
    // doc_markdown : exige des backticks autour de SQLite/URL/WebSocket/Postgres...
    // dans les docs. Bruit pur sur de la prose technique — on l'eteint sciemment.
    clippy::doc_markdown,
    // duration_suboptimal_units : prefere from_mins/from_hours. `from_secs(N)` reste
    // parfaitement clair pour des seuils exprimes en secondes — on garde la seconde.
    clippy::duration_suboptimal_units
)]

pub mod config;
pub mod db;
pub mod ingest;
