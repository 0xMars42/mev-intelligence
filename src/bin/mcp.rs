//! Serveur MCP `mcp` — expose l'intelligence MEV comme outils (P3.6).
//!
//! Implémente le Model Context Protocol (JSON-RPC 2.0 sur stdio, messages
//! délimités par des newlines). Un client LLM (Claude Desktop / Claude Code,
//! avec TES credentials) s'y connecte et appelle les outils pour interroger la
//! data MEV et rédiger des fiches bot — **sans aucune clé API dans le projet**.
//!
//! Outils exposés : `db_summary`, `top_bots`, `bot_profile`.
//!
//! IMPORTANT : stdout est réservé au JSON-RPC. Tous les logs vont sur stderr.
//!
//! Usage (enregistrement côté client) : cf. README, section "MCP server".

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
    clippy::too_many_lines
)]

use eyre::Result;
use mev_intelligence::config::Config;
use mev_intelligence::db::Db;
use serde_json::{Value, json};
use std::fmt::Write as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const PROTOCOL_VERSION: &str = "2024-11-05";
const CAVEAT: &str = "NOTE: classes are heuristic hypotheses, not ground truth; \
weth_volume_eth is a SIZE proxy (ETH injected on WETH-in swaps), NOT realised P&L; \
only the public mempool is observed (private orderflow is invisible).";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if let Err(e) = dotenvy::dotenv()
        && !e.not_found() {
            eprintln!("[warn] .env : {e}");
        }
    // IMPORTANT : logs sur STDERR — STDOUT est reserve au JSON-RPC.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    let db = Db::connect(&cfg.database_url).await?;
    info!(db = %cfg.database_url, "serveur MCP mev-intelligence pret (stdio JSON-RPC)");

    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!(err = %e, "JSON-RPC invalide recu, ignore");
                continue;
            }
        };
        if let Some(resp) = handle(&db, &req).await {
            let s = serde_json::to_string(&resp)?;
            stdout.write_all(s.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    info!("stdin ferme — arret du serveur MCP");
    Ok(())
}

/// Dispatch d'un message JSON-RPC. Renvoie `None` pour les notifications
/// (pas d'`id`, donc pas de réponse attendue).
async fn handle(db: &Db, req: &Value) -> Option<Value> {
    let method = req.get("method")?.as_str()?;
    let id = req.get("id").cloned();
    match method {
        "initialize" => {
            let version = req
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            Some(ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "mev-intelligence",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            ))
        }
        "notifications/initialized" => None,
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": tool_defs() }))),
        "tools/call" => Some(handle_tools_call(db, id, req).await),
        _ => Some(err(id, -32601, "method not found")),
    }
}

async fn handle_tools_call(db: &Db, id: Option<Value>, req: &Value) -> Value {
    let params = req.get("params");
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    match call_tool(db, name, &args).await {
        Ok(text) => ok(
            id,
            json!({ "content": [ { "type": "text", "text": text } ] }),
        ),
        Err(e) => ok(
            id,
            json!({
                "content": [ { "type": "text", "text": format!("error: {e}") } ],
                "isError": true
            }),
        ),
    }
}

async fn call_tool(db: &Db, name: &str, args: &Value) -> Result<String> {
    match name {
        "db_summary" => db_summary(db).await,
        "top_bots" => {
            let metric = args
                .get("metric")
                .and_then(Value::as_str)
                .unwrap_or("volume");
            let limit = args
                .get("limit")
                .and_then(Value::as_i64)
                .unwrap_or(10)
                .clamp(1, 100);
            top_bots(db, metric, limit).await
        }
        "bot_profile" => {
            let address = args
                .get("address")
                .and_then(Value::as_str)
                .unwrap_or_default();
            bot_profile(db, address).await
        }
        other => eyre::bail!("outil inconnu: {other}"),
    }
}

// --- Rendu texte des outils (ce que le LLM lit) -----------------------------

async fn db_summary(db: &Db) -> Result<String> {
    let pending = db.count_pending().await?;
    let outcomes = db.count_outcomes().await?;
    let operators = db.count_operators().await?;
    let stats = db.count_bot_stats().await?;
    let breakdown = db.bot_class_breakdown().await?;

    let mut s = String::new();
    writeln!(s, "MEV intelligence DB summary:")?;
    writeln!(s, "- pending_tx (captured swaps): {pending}")?;
    writeln!(s, "- tx_outcome (validated): {outcomes}")?;
    writeln!(s, "- operators (multi-wallet clusters): {operators}")?;
    writeln!(s, "- addresses with stats: {stats}")?;
    if breakdown.is_empty() {
        writeln!(s, "- bot classes: (run the `classify` binary first)")?;
    } else {
        let parts: Vec<String> = breakdown.iter().map(|(c, n)| format!("{c}={n}")).collect();
        writeln!(s, "- bot classes: {}", parts.join(", "))?;
    }
    write!(s, "\n{CAVEAT}")?;
    Ok(s)
}

async fn top_bots(db: &Db, metric: &str, limit: i64) -> Result<String> {
    let by_volume = !metric.eq_ignore_ascii_case("activity");
    let rows = db.top_bots(by_volume, limit).await?;
    let header = if by_volume {
        "volume (ETH injected)"
    } else {
        "activity (#swaps)"
    };

    let mut s = String::new();
    writeln!(s, "Top {} bots by {header}:", rows.len())?;
    for (i, (addr, class, tx, vol, sr, rr)) in rows.iter().enumerate() {
        writeln!(
            s,
            "#{:<2} {}  class={}  swaps={}  volume={:.4} ETH  success={:.0}%  revert={:.0}%",
            i + 1,
            addr,
            class.as_deref().unwrap_or("?"),
            tx,
            vol,
            sr * 100.0,
            rr * 100.0
        )?;
    }
    if rows.is_empty() {
        writeln!(
            s,
            "(no stats yet — run the daemon, then the `leaderboard` binary)"
        )?;
    }
    write!(s, "\n{CAVEAT}")?;
    Ok(s)
}

async fn bot_profile(db: &Db, address: &str) -> Result<String> {
    if address.is_empty() {
        eyre::bail!("argument 'address' requis (0x... lowercase)");
    }
    let Some(p) = db.bot_profile(address).await? else {
        return Ok(format!(
            "Aucun profil pour {address} (address inconnue, ou stats pas encore calculees)."
        ));
    };

    let mut s = String::new();
    writeln!(s, "Bot profile: {}", p.address)?;
    writeln!(
        s,
        "- class: {}",
        p.class.as_deref().unwrap_or("(unclassified)")
    )?;
    writeln!(
        s,
        "- activity: {} swaps, {} distinct token(s)",
        p.tx_count, p.distinct_tokens
    )?;
    writeln!(
        s,
        "- volume (size proxy): {:.4} ETH injected (WETH-in)",
        p.weth_volume_eth
    )?;
    if let Some(w) = p.weth_in_ratio {
        writeln!(s, "- buying ratio (WETH-in): {:.0}%", w * 100.0)?;
    }
    if let Some(g) = p.avg_max_fee_gwei {
        writeln!(s, "- avg gas: {g:.2} gwei")?;
    }
    writeln!(
        s,
        "- outcomes: {} validated ({} success, {} reverted, {} not-mined) | success {:.0}% | revert {:.0}%",
        p.validated,
        p.mined_success,
        p.mined_reverted,
        p.not_mined,
        p.success_rate * 100.0,
        p.revert_rate * 100.0
    )?;
    if let (Some(id), Some(size), Some(shared)) =
        (p.operator_id, p.operator_size, p.operator_shared_tokens)
    {
        writeln!(
            s,
            "- operator cluster: #{id} ({size} addresses, {shared} shared evidence tokens)"
        )?;
    } else {
        writeln!(
            s,
            "- operator cluster: none (no multi-wallet co-occurrence found)"
        )?;
    }
    writeln!(
        s,
        "- active window (epoch ms): {} .. {}",
        p.first_seen_ms, p.last_seen_ms
    )?;
    write!(s, "\n{CAVEAT}")?;
    Ok(s)
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "db_summary",
            "description": "Overview of the MEV intelligence database: row counts and bot-class breakdown.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "top_bots",
            "description": "Leaderboard of bots by 'volume' (ETH injected) or 'activity' (#swaps), with their class.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "metric": {
                        "type": "string",
                        "enum": ["volume", "activity"],
                        "description": "Ranking metric (default: volume)."
                    },
                    "limit": { "type": "integer", "description": "Max rows, 1-100 (default 10)." }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "bot_profile",
            "description": "Full profile of one address: class, activity, volume, outcome rates, operator cluster.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "address": { "type": "string", "description": "0x... address (lowercase)." }
                },
                "required": ["address"],
                "additionalProperties": false
            }
        }
    ])
}

fn ok(id: Option<Value>, result: Value) -> Value {
    // Construction manuelle pour que `result` soit move sans ambiguïté.
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), Value::from("2.0"));
    obj.insert("id".into(), id.unwrap_or(Value::Null));
    obj.insert("result".into(), result);
    Value::Object(obj)
}

fn err(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": { "code": code, "message": message }
    })
}
