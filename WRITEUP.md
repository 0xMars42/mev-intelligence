# Profiling MEV bots from the public mempool

> A short research note on what [`mev-intelligence`](./README.md) finds when
> pointed at the live Ethereum public mempool. Numbers below come from a small,
> representative sample (a few hours of capture) — the methodology is the point,
> not the magnitude. Everything here is reproducible from the repo.

## TL;DR

`mev-intelligence` ingests the Ethereum **public mempool**, records what each
transaction actually did on-chain, and turns raw swaps into *profiles of the
actors*: which wallets belong to one operator, what strategy each bot runs, and
how often it wins or loses. On a first live run it already surfaces bots that pay
gas and **revert on 100% of their attempts** — i.e. burn ETH losing every MEV
race.

## Why

Detecting MEV live is easy and ephemeral. The hard, valuable part is
*understanding the actors* — which wallets are one operator, what each bot is
doing, and whether it actually makes money. That needs accumulated data plus an
analysis layer; it's the job of teams like Flashbots, EigenPhi, and the MEV desks
at trading firms.

## The pipeline

Each stage is a small Rust binary over one SQLite database:

1. **Ingest** — subscribe to full pending-tx bodies over WebSocket, filter to
   known DEX routers (Uni V2/V3, Universal Router, 1inch), decode the swap,
   persist it. Reuses the earlier [`eth-mempool-watcher`](https://github.com/0xMars42/eth-mempool-watcher)
   as a *library* — no duplication.
2. **Outcome** — a periodic pass reads each tx's receipt and freezes its fate:
   mined-success / mined-reverted / dropped.
3. **Operators** — cluster wallets that hit the same *subject* tokens in the same
   time windows across multiple distinct tokens (union-find on a co-occurrence
   graph). Quote tokens (WETH/USDC/USDT/DAI) are excluded from the signal, so a
   wallet selling many tokens (whose `token_out` is always WETH) isn't mistaken
   for a single-token bot.
4. **Classify** — a behavioural fingerprint (token diversity, buy ratio, gas
   aggression, revert rate) → a bot type.
5. **Analytics** — per-bot stats + leaderboards (volume, activity, success/revert).
6. **Interfaces** — an **MCP server** (so your own Claude queries the data and
   writes reports — *no API key in the project*) and an **axum** web dashboard +
   JSON API.

## First findings

A representative live sample (tens of thousands of pending txs seen; **~0.5%**
were whitelisted DEX router calls; a few hundred swaps captured; a couple of
hundred addresses profiled):

- **Gas burned on lost races.** The most useful board is "revert rate": several
  addresses reverted on **100% of their (3–4) attempts**, plus others at 50–60%.
  A bot that reverts every time is paying gas to lose — exactly what the platform
  is built to flag. (Small samples; the point is that the signal is surfaced and
  ranked.)
- **Most flow is opaque.** **~61% of captured swaps carried no decoded token** —
  the Universal Router (~51%, only the outer envelope is decoded), 1inch (always
  undecoded today), plus a few unknown selectors. So the token-level analysis sees
  roughly a *third* of the flow. An honest blind spot of public-mempool analysis
  (and much MEV is private anyway).
- **Operators emerge once quote tokens are excluded.** Treating WETH/stables as
  "subject tokens" would cluster everyone who sells to WETH; excluding them lets a
  first genuine multi-wallet operator cluster surface. Bigger clusters need more
  data.

## What this deliberately does NOT claim

- **Sandwich / JIT** detection — needs intra-block ordering and Universal-Router
  inner-call decoding, not done here.
- **Realised P&L** — "volume" is ETH *injected*, a size proxy, not profit. True
  P&L needs round-trip matching and pricing of received tokens (incl. memecoins
  with no reliable price).
- **Private orderflow** is invisible. Classes are heuristic hypotheses, and every
  tool output carries that caveat.

## Why it's interesting as engineering

Async ingestion in Rust with WebSocket reconnection; a previous project reused as
a crate; a graph + union-find entity layer; SQLite now with a documented Postgres
scale-path; `clippy::pedantic + nursery` clean across the crate. The part worth a
second look: a **from-scratch MCP server** so any LLM client drives the analysis
with the operator's own credentials — no API key — plus an axum dashboard.

## Reproduce it

```bash
git clone https://github.com/0xMars42/mev-intelligence.git
cd mev-intelligence
cargo run --release      # daemon: ingest + validate
cargo run --bin cluster  # operators
cargo run --bin classify # bot types
cargo run --bin leaderboard
cargo run --bin web      # dashboard at http://127.0.0.1:8080
```

Feedback from people who actually run MEV infra is very welcome — especially on
the clustering heuristics. Next: accumulate longer (real operator clusters), add
Transfer-log P&L, decode Universal-Router inner calls.
