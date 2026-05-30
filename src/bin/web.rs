//! Binaire `web` — dashboard HTML + API JSON (P3.7), sur axum.
//!
//! Sert un petit dashboard d'exploration (résumé, leaderboards) et une API JSON
//! sur la même donnée que les autres binaires :
//!   GET /                  -> dashboard HTML
//!   GET /api/summary       -> compteurs + répartition des classes
//!   GET /api/top?metric=&limit=  -> leaderboard (volume | activity)
//!   GET /api/bot/{address} -> profil complet d'une address
//!   GET /api/operators     -> clusters d'opérateurs
//!
//! Usage : `cargo run --bin web` (écoute 127.0.0.1:8080 ; `MEV_WEB_ADDR` pour
//! surcharger).

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
    clippy::doc_markdown,
    clippy::too_many_lines,
    // axum : les extracteurs (State, Query, Path, Json) sont passés par valeur
    // par contrat du framework.
    clippy::needless_pass_by_value,
    // option_if_let_else : le `match Some/None` est plus lisible que map_or_else ici.
    clippy::option_if_let_else
)]

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use eyre::Result;
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write as _;
use tracing::info;
use tracing_subscriber::EnvFilter;

const DEFAULT_ADDR: &str = "127.0.0.1:8080";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    let db = Db::connect(&cfg.database_url).await?;
    let addr = std::env::var("MEV_WEB_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/summary", get(api_summary))
        .route("/api/top", get(api_top))
        .route("/api/bot/{address}", get(api_bot))
        .route("/api/operators", get(api_operators))
        .with_state(db);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("dashboard MEV intelligence sur http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Map toute erreur DB en 500 (le détail n'a pas à fuiter au client).
fn ise<E>(_e: E) -> StatusCode {
    StatusCode::INTERNAL_SERVER_ERROR
}

// --- API JSON ---------------------------------------------------------------

async fn api_summary(State(db): State<Db>) -> Result<Json<Value>, StatusCode> {
    let breakdown = db.bot_class_breakdown().await.map_err(ise)?;
    let classes: Value = breakdown
        .into_iter()
        .map(|(c, n)| json!({ "class": c, "count": n }))
        .collect();
    Ok(Json(json!({
        "pending_tx": db.count_pending().await.map_err(ise)?,
        "tx_outcome": db.count_outcomes().await.map_err(ise)?,
        "operators": db.count_operators().await.map_err(ise)?,
        "addresses_with_stats": db.count_bot_stats().await.map_err(ise)?,
        "bot_classes": classes
    })))
}

#[derive(Deserialize)]
struct TopQuery {
    metric: Option<String>,
    limit: Option<i64>,
}

async fn api_top(
    State(db): State<Db>,
    Query(q): Query<TopQuery>,
) -> Result<Json<Value>, StatusCode> {
    let by_volume = q.metric.as_deref() != Some("activity");
    let limit = q.limit.unwrap_or(10).clamp(1, 100);
    let rows = db.top_bots(by_volume, limit).await.map_err(ise)?;
    let bots: Value = rows
        .into_iter()
        .map(|(address, class, swaps, vol, sr, rr)| {
            json!({
                "address": address, "class": class, "swaps": swaps,
                "weth_volume_eth": vol, "success_rate": sr, "revert_rate": rr
            })
        })
        .collect();
    Ok(Json(json!({
        "metric": if by_volume { "volume" } else { "activity" },
        "bots": bots
    })))
}

async fn api_bot(
    State(db): State<Db>,
    Path(address): Path<String>,
) -> Result<Response, StatusCode> {
    match db.bot_profile(&address).await.map_err(ise)? {
        Some(profile) => Ok(Json(profile).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown address or no stats yet" })),
        )
            .into_response()),
    }
}

async fn api_operators(State(db): State<Db>) -> Result<Json<Value>, StatusCode> {
    let ops = db.list_operators().await.map_err(ise)?;
    let operators: Value = ops
        .into_iter()
        .map(|(id, size, shared, first, last, addresses)| {
            json!({
                "id": id, "size": size, "shared_tokens": shared,
                "first_seen_ms": first, "last_seen_ms": last, "addresses": addresses
            })
        })
        .collect();
    Ok(Json(json!({ "operators": operators })))
}

// --- Dashboard HTML ---------------------------------------------------------

async fn dashboard(State(db): State<Db>) -> Result<Html<String>, StatusCode> {
    let pending = db.count_pending().await.map_err(ise)?;
    let outcomes = db.count_outcomes().await.map_err(ise)?;
    let operators = db.count_operators().await.map_err(ise)?;
    let stats = db.count_bot_stats().await.map_err(ise)?;
    let breakdown = db.bot_class_breakdown().await.map_err(ise)?;
    let top_vol = db.top_bots(true, 10).await.map_err(ise)?;
    let top_act = db.top_bots(false, 10).await.map_err(ise)?;

    let classes = if breakdown.is_empty() {
        "(run the `classify` binary)".to_string()
    } else {
        breakdown
            .iter()
            .map(|(c, n)| format!("{c}: {n}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };

    let mut vol_rows = String::new();
    for (i, (addr, class, tx, vol, _sr, rr)) in top_vol.iter().enumerate() {
        let _ = write!(
            vol_rows,
            "<tr><td>{}</td><td class=mono>{addr}</td><td>{}</td><td>{tx}</td><td>{vol:.4}</td><td>{:.0}%</td></tr>",
            i + 1,
            class.as_deref().unwrap_or("?"),
            rr * 100.0
        );
    }
    let mut act_rows = String::new();
    for (i, (addr, class, tx, vol, _sr, _rr)) in top_act.iter().enumerate() {
        let _ = write!(
            act_rows,
            "<tr><td>{}</td><td class=mono>{addr}</td><td>{}</td><td>{tx}</td><td>{vol:.4}</td></tr>",
            i + 1,
            class.as_deref().unwrap_or("?")
        );
    }
    if vol_rows.is_empty() {
        vol_rows.push_str("<tr><td colspan=6 class=sub>no stats yet — run the daemon then the leaderboard binary</td></tr>");
    }

    let html = format!(
        r#"<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width, initial-scale=1">
<title>MEV intelligence</title>
<style>
 body{{background:#0d1117;color:#c9d1d9;font-family:system-ui,sans-serif;margin:2rem;max-width:980px}}
 h1{{color:#58a6ff;margin-bottom:.2rem}} h2{{color:#58a6ff;margin-top:2rem}}
 table{{border-collapse:collapse;width:100%;margin:.5rem 0}}
 th,td{{border:1px solid #30363d;padding:.35rem .6rem;text-align:left;font-size:.9rem}}
 th{{background:#161b22}} .mono{{font-family:ui-monospace,monospace;font-size:.78rem}}
 .sub{{color:#8b949e;font-size:.9rem}} a{{color:#58a6ff}}
 .cards span{{display:inline-block;background:#161b22;border:1px solid #30363d;border-radius:6px;padding:.4rem .8rem;margin:.2rem .3rem .2rem 0}}
</style></head>
<body>
<h1>MEV intelligence</h1>
<p class=sub>Ethereum public mempool. Classes are heuristic hypotheses; volume is ETH injected on WETH-in swaps (a <b>size proxy</b>, not realised P&amp;L); private orderflow is invisible.</p>
<div class=cards>
 <span>pending_tx <b>{pending}</b></span><span>validated <b>{outcomes}</b></span>
 <span>operators <b>{operators}</b></span><span>addresses w/ stats <b>{stats}</b></span>
</div>
<p class=sub>classes: {classes}</p>
<h2>Top bots by volume (ETH injected)</h2>
<table><tr><th>#</th><th>address</th><th>class</th><th>swaps</th><th>volume ETH</th><th>revert%</th></tr>{vol_rows}</table>
<h2>Top bots by activity (#swaps)</h2>
<table><tr><th>#</th><th>address</th><th>class</th><th>swaps</th><th>volume ETH</th></tr>{act_rows}</table>
<p class=sub>JSON API: <a href="/api/summary">/api/summary</a> · <a href="/api/top?metric=volume">/api/top?metric=volume</a> · /api/bot/&lt;address&gt; · <a href="/api/operators">/api/operators</a></p>
</body></html>"#
    );
    Ok(Html(html))
}
