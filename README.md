# mev-intelligence

> A platform that **ingests, persists and (soon) classifies** MEV bot activity
> on Ethereum mainnet — built in Rust. It turns a real-time mempool radar into a
> system with **memory**: every decoded pending swap is stored, so operators can
> be clustered, their strategies fingerprinted, and their P&L estimated.

> **Status:** P3.1–P3.6 — full pipeline: live ingestion + persistence, on-chain
> outcomes, **operator** clustering, behavioural **classification**, per-bot
> **analytics/leaderboards**, and an **MCP server** that exposes it all to your
> own Claude (Desktop/Code) — no API key — to generate natural-language reports.

---

## Why this exists

Detecting MEV activity live is useful but ephemeral. The value is in
**understanding the actors**: which addresses belong to the same operator, what
strategy each bot runs (sniper / sandwich / arb / JIT / copy-trader), how
profitable they are. That requires *accumulated* data, not a rolling buffer.

This project is the data + intelligence layer on top of two earlier Rust
projects:

- **[`eth-mempool-watcher`](https://github.com/0xMars42/eth-mempool-watcher)** —
  real-time mempool decoding + MEV pattern detection. **Reused here as a
  library** (`routers` + `decode`), not duplicated.
- **[`base-arb-scanner`](https://github.com/0xMars42/base-arb-scanner)** —
  cross-DEX arbitrage pricing with on-chain Quoter validation.

## What it does today (P3.1–P3.2)

```
WebSocket pending tx  ─►  router whitelist filter  ─►  decode swap (P2 lib)
                                                            │
                                                            ▼
                                          pending_row (pure mapping)
                                                            │
                                                            ▼
                                        SQLite (sqlx)  ── pending_tx table
```

- Subscribes to **full pending tx bodies** over WebSocket (no polling, no API key).
- Filters to known DEX routers (Uniswap V2/V3, Universal Router, 1inch v6).
- Decodes the swap and writes a flat `pending_tx` row, **deduplicated on tx hash**.
- Embedded SQL migrations run at startup (no `sqlx-cli` needed).
- A periodic **validation pass** reads each tx's receipt once it is old enough
  and records the real outcome (mined / reverted / dropped) in `tx_outcome`.
  The swap+outcome dataset is `pending_tx JOIN tx_outcome USING (hash)`.

## Design choices

- **Reuses P2 as a crate** (`eth-mempool-watcher = { path = ".." }`) — the decode
  machinery is already live-validated; P3 consumes it.
- **Pure mapping layer** (`ingest::pending_row`) — `DecodedSwap` → DB row is a
  pure function, unit-tested without any network or database.
- **SQLite first, Postgres-portable** — zero infra to run locally; the only
  SQLite-specific SQL (`INSERT OR IGNORE`) is isolated in `db.rs`. Postgres is the
  documented scale-path, ClickHouse beyond that.
- **U256 amounts stored as decimal text** — a 256-bit amount doesn't fit a 64-bit
  integer column; text keeps them exact and portable.

## Quick start

Requires Rust (edition 2024). No database server needed — SQLite is embedded.

```bash
git clone https://github.com/0xMars42/mev-intelligence.git
cd mev-intelligence
cargo run --release            # daemon: ingests + validates into ./mev_intel.db
cargo run --bin cluster        # on-demand: cluster wallets into operators
cargo run --bin classify       # on-demand: assign each address a bot type
cargo run --bin leaderboard    # on-demand: per-bot stats + top-10 leaderboards
cargo run --bin mcp            # MCP server (stdio) — register it in your Claude
```

Inspect what's been captured (any SQLite client):

```sql
SELECT router, kind, count(*) FROM pending_tx GROUP BY 1, 2 ORDER BY 3 DESC;
SELECT token_out, count(*) AS n FROM pending_tx
  WHERE token_out IS NOT NULL GROUP BY 1 ORDER BY n DESC LIMIT 10;
```

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `ETH_WS_URL` | `wss://ethereum-rpc.publicnode.com` | WebSocket RPC (pending full tx) |
| `DATABASE_URL` | `sqlite://mev_intel.db` | sqlx connection string |
| `MEV_STATS_SECS` | `5` | Cumulative stats log interval |

## MCP server — drive it from your own Claude (no API key)

The `mcp` binary is a [Model Context Protocol](https://modelcontextprotocol.io)
server (JSON-RPC over stdio). It exposes the intelligence as tools — `db_summary`,
`top_bots`, `bot_profile` — so **your** Claude (Desktop or Code, your credentials)
can query the data and write natural-language bot reports. No `ANTHROPIC_API_KEY`
lives in this repo; the inference runs in your Claude client. (MCP works the other
way round from an API call: Claude *calls the server's tools*, the server returns
data — the report is written by your Claude.)

Build once: `cargo build --release` → binary at `target/release/mcp`. Make sure the
DB is populated first (run the daemon, then `cluster` / `classify` / `leaderboard`).

**Claude Desktop (Windows + WSL)** — add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "mev-intelligence": {
      "command": "wsl",
      "args": ["-d", "Ubuntu", "--", "bash", "-lc",
               "cd ~/projects/mev-intelligence && exec ./target/release/mcp"]
    }
  }
}
```

**Claude Code (inside WSL):**

```bash
claude mcp add mev-intelligence -- bash -lc 'cd ~/projects/mev-intelligence && exec ./target/release/mcp'
```

Then ask your Claude, e.g. *"Use mev-intelligence: summarize the DB, then write a
profile of the top bot by volume."* It calls the tools and reasons over the result.
Logs go to stderr; stdout is reserved for the protocol.

## Roadmap

| Phase | Status | What |
|---|---|---|
| P3.1 | ✅ | Live ingestion + persistence (`pending_tx`) |
| P3.2 | ✅ | Receipts → outcome (`tx_outcome`): swap + mined/reverted/dropped dataset |
| P3.3 | ✅ | Entity layer: cluster wallets into operators by co-occurrence (`operator`) |
| P3.4 | ✅ | Behavioural classification → bot taxonomy (`bot_class`) |
| P3.5 | ✅ | Per-bot analytics + leaderboards (`bot_stats`): volume / activity / rates |
| P3.6 | ✅ | MCP server: exposes the intelligence to your own Claude (no API key) |
| P3.7 | 📋 | Dashboard / query API |

## License

MIT.

## Author

[0xMars42](https://github.com/0xMars42) — portfolio project for **Rust / EVM /
MEV research** roles.
