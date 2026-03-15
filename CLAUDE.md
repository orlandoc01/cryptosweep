# Crypto Sweep — Automated USDC Deposit Detection & Coinbase Liquidation

## Project Overview

Cron-based tool that polls block explorers across 5 EVM chains (Ethereum,
Base, Arbitrum, Polygon, Optimism) for USDC transfers to configured deposit
addresses. Only transfers with sufficient block confirmations are considered
finalized — a receive is eligible when
`current_block - receive.block_number >= confirmation_blocks` (default: 14
blocks per chain). Eligible transfers are sold on Coinbase and the proceeds
are withdrawn via a linked Payment Method on Coinbase.com. Any Payment Method
that supports withdrawals works.

---

## Stack & Environment

- **Language:** Rust (edition 2024)
- **Async runtime:** tokio (multi-thread)
- **Target:** macOS or Linux (uses cron and POSIX file locking; Windows not supported)
- **Core crates:**
  - `reqwest` — HTTP client (Coinbase API, block explorer APIs)
  - `serde` / `serde_json` — serialization
  - `tokio` — async runtime
  - `rust_decimal` — financial arithmetic (never use f64 for money)
  - `chrono` — date/time handling
  - `thiserror` — ergonomic error types
  - `tracing` + `tracing-subscriber` — structured logging
  - `alloy` — typed Ethereum JSON-RPC client (block heights, event logs)
  - `futures` — async combinators (join_all for concurrent chain scans)
  - `tokio-stream` — stream adapters for tokio channels (ReceiverStream for cbworker pipeline)
  - `teloxide` — Telegram bot API (error notifications)
  - `p256` — ECDSA P-256 for Coinbase CDP API JWT authentication (ES256)
  - `uuid` — UUIDv4 for Coinbase JWT nonces only
  - `fs2` — cross-platform file locking
  - `strum` — derive Display, EnumIter, etc. for enums
  - `garde` — declarative struct validation (deposit address format, non-empty lists)

---

## Safety Rules (NEVER VIOLATE)

1. **No private key access:** The system NEVER holds or manages private keys.
   Ledger-signed transactions are always manual.
2. **Amount threshold:** If any detected USDC deposit exceeds a configurable
   maximum (e.g., $10,000), do NOT attempt liquidation. Notify the user via
   Telegram and persist a `BankPayment` in `Failed` state for audit. Catches
   data anomalies.
3. **Log everything:** All actions, API responses, and errors must be logged.
   Every financial operation must be auditable.
4. **Deduplication:** A deposit transaction hash already present in
   `processed_tx_hashes` must never trigger a second sell + withdraw, even if
   the binary crashes and restarts mid-flow.

---

## Execution Model

**Cron-based, not a daemon.** The binary runs every 15 minutes via cron, does
its work, and exits.

Each invocation follows this pattern:

1. Load config from `~/.cryptosweep/config.toml`.
2. Set up Telegram notifier (used for lock/state failure alerts).
3. Acquire a file lock (`/tmp/cryptosweep.lock`). If locked, notify
   via Telegram and exit.
4. Load persisted state from `~/.cryptosweep/state.json`. If loading
   fails, notify via Telegram and exit.
5. **Deposit check pass** (runs every invocation, pipelined):
   Chain discovery and Coinbase execution run concurrently via
   `tokio::join!` over an `mpsc` channel. The first eligible deposit
   starts sell+withdraw while other chains are still scanning.
   - **Discovery** (concurrent across chains):
     - For each chain, fetch the current block height.
     - Use `last_seen_block[chain]` as `startblock` if present;
       otherwise use `start_blocks[chain]` from config.
     - If neither exists, skip that chain with a warning.
     - For each configured deposit address, fetch USDC receives.
     - For each receive: skip if dust (< 1 USDC), already processed,
       or not yet finalized. Eligible deposits are sent to the worker
       channel as `EligibleDeposit`.
     - After scanning, update `last_seen_block[chain]`.
   - **CbWorker** (processes channel, up to 5 concurrent jobs):
     - For each `EligibleDeposit` received from the channel:
       - If amount exceeds `max_auto_amount`: persist `BankPayment(Failed)`,
         notify user.
       - Otherwise: sell USDC on Coinbase, wait 30s for settlement,
         initiate withdrawal via Payment Method, persist
         `BankPayment(Initiated)`, notify user.
     - Worker exits when all senders drop (discovery complete).
6. **Prune stale entries:** Remove `BankPayment` records where `processed_at`
   is older than 90 days. Pruned payments also have their `deposit_tx_hash`
   removed from `processed_tx_hashes` — safe because `last_seen_block`
   ensures blocks that old are never re-queried.
7. Persist state to disk.
8. **Error notification:** If any errors occurred during the run, send a
   single Telegram message summarizing all errors.
9. Release lock, exit.

**Idempotency:** Running the binary twice with no external changes produces
the same result. Deposit processing is gated on `processed_tx_hashes`. The
block confirmation filter is stateless — it re-evaluates on every run based
on current block height.

**Deployment:** The binary is symlinked from the build output so that
`cargo build --release` is the only deploy step:
```
sudo ln -sf "$(pwd)/target/release/cryptosweep" /usr/local/bin/cryptosweep
```

**Cron entry** (runs every 15 minutes):
```
*/15 * * * * /usr/local/bin/cryptosweep >> /var/log/cryptosweep.log 2>&1
```

---

## Project Structure

```
cryptosweep/
├── Cargo.toml
├── CLAUDE.md                    ← this file
├── config.sample.toml           ← sample config (copy to ~/.cryptosweep/config.toml)
├── src/
│   ├── main.rs                  ← tokio entrypoint: lock → load → deposit check → persist → exit
│   ├── config.rs                ← config loading from ~/.cryptosweep/config.toml
│   ├── types.rs                 ← shared types, enums, error definitions
│   ├── explorer.rs              ← BlockExplorer trait + JSON-RPC impl (via alloy)
│   ├── coinbase/
│   │   ├── mod.rs               ← CoinbaseClient trait + live implementation
│   │   ├── auth.rs              ← JWT ES256 signing (build_jwt, parse_pem_key)
│   │   └── models.rs            ← Coinbase API request/response types
│   ├── notifier/
│   │   └── mod.rs               ← Notifier trait + Telegram impl (error/success alerts)
│   ├── cbworker.rs              ← CbWorker: channel-driven Coinbase sell + withdraw execution
│   ├── orchestrator.rs          ← DepositCheck (pipelined discovery) + ChainDepositCheck (per-chain scan)
│   ├── persistence.rs           ← PersistedState load/save, file locking, stale entry pruning
│   └── tests/
│       ├── mod.rs
│       ├── cassette.rs           ← HTTP response cassette framework for smoke tests
│       ├── coinbase_mock.rs      ← Mock impls of BlockExplorer, CoinbaseClient, Notifier
│       └── fixtures/
│           └── *.cassette.json   ← HTTP response fixtures for API smoke tests
```

Single crate with module directories.

---

## Core Type Definitions

All shared types, traits, and error definitions live in `src/types.rs`.
Configuration types live in `src/config.rs`.

---

## Data Flow (per cron invocation)

```
┌────────────────────────────────────────────────────────────────┐
│  CRON INVOCATION (every 15 minutes)                            │
│                                                                │
│  1. Load config from ~/.cryptosweep/config.toml                │
│  2. Set up Telegram notifier                                   │
│  3. Acquire lock file (or notify + exit if locked)             │
│  4. Load state from ~/.cryptosweep/state.json                  │
│                                                                │
│  ┌──── DEPOSIT CHECK PASS (pipelined via tokio::join!) ─────┐  │
│  │                                                          │  │
│  │  DISCOVERY (concurrent chain scans)                      │  │
│  │  ──────────────────────────────────                      │  │
│  │  For each chain (concurrently):                          │  │
│  │    → explorer.get_block_height() → current_block         │  │
│  │    → startblock = last_seen_block[chain]                 │  │
│  │               ?? start_blocks[chain] from config         │  │
│  │               ?? skip chain (warn if neither exists)     │  │
│  │                                                          │  │
│  │    For each configured deposit address:                  │  │
│  │      → explorer.fetch_usdc_receives(addr, startblock)    │  │
│  │                                                          │  │
│  │    For each receive:                                     │  │
│  │      → skip if tx_hash in processed_tx_hashes            │  │
│  │      → skip if amount < 1.0 USDC (dust)                  │  │
│  │      → skip if current_block - receive.block_number      │  │
│  │           < confirmation_blocks[chain] (default: 14)     │  │
│  │      → send EligibleDeposit to worker channel            │  │
│  │                                                          │  │
│  │    → update last_seen_block[chain] = confirmed_frontier  │  │
│  │                                                          │  │
│  │              ┌──── mpsc channel (bounded: 32) ────┐      │  │
│  │              │       EligibleDeposit →            │      │  │
│  │              └────────────────────────────────────┘      │  │
│  │                                                          │  │
│  │  CBWORKER (up to 5 concurrent jobs)                      │  │
│  │  ──────────────────────────────────                      │  │
│  │  For each EligibleDeposit from channel:                  │  │
│  │    → if amount > max_auto_amount:                        │  │
│  │        persist BankPayment(Failed), notify user          │  │
│  │    → else:                                               │  │
│  │        sell_usdc(amount)                                 │  │
│  │        wait 30s for settlement                           │  │
│  │        withdraw_usd(proceeds, payment_method_id)         │  │
│  │        persist BankPayment(Initiated), notify user       │  │
│  │                                                          │  │
│  │  Worker exits when all senders drop (discovery complete) │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                │
│  5. Prune bank payments older than 90 days. Remove pruned      │
│     tx hashes from processed_tx_hashes.                        │
│  6. Persist state to disk                                      │
│  7. If any errors occurred during the run, send a single       │
│     Telegram alert summarizing all errors.                     │
│  8. Release lock, exit                                         │
└────────────────────────────────────────────────────────────────┘
```

---

## Coinbase API Reference

**Authentication**: JWT ES256 — NOT legacy HMAC-SHA256. Each request requires a
freshly generated JWT, valid for 120 seconds and scoped to the exact endpoint.
- Docs: <https://docs.cdp.coinbase.com/coinbase-app/authentication-authorization/api-key-authentication>
- Config `coinbase_api_key`: CDP key name (`organizations/{org_id}/apiKeys/{key_id}`)
- Config `coinbase_api_secret`: PKCS8 PEM-encoded ECDSA P-256 private key

**JWT structure**:
- Header: `{"alg":"ES256","typ":"JWT","kid":"{key_name}","nonce":"{uuid4_hex}"}`
- Claims: `{"sub":"{key_name}","iss":"cdp","nbf":{ts},"exp":{ts+120},"uri":"{METHOD} api.coinbase.com{path}"}`
- Signed with ECDSA P-256 (SHA-256 hash + compact r||s 64-byte signature)
- Request header: `Authorization: Bearer {jwt}`

**Endpoints used**:

| Operation | Method | Path |
|-----------|--------|------|
| List accounts (find USDC + USD account IDs) | `GET` | `/v2/accounts` |
| Request USDC→USD conversion quote | `POST` | `/api/v3/brokerage/convert/quote` |
| Commit conversion trade | `POST` | `/api/v3/brokerage/convert/trade/{trade_id}` |
| Initiate withdrawal to Payment Method | `POST` | `/v2/accounts/{usd_account_id}/withdrawals` |

**Sell flow:** USDC↔USD is a 1:1 stablecoin conversion via the v3 brokerage
convert API (quote → commit), not a market trade.

**Withdrawal flow:** `commit: true` in the request body commits in one step.
The response `data.id` is the withdrawal transfer UUID.

---

## Block Explorer Reference

Direct JSON-RPC calls to free public EVM nodes via the `alloy` crate.
No API keys required. Uses standard methods:
- `eth_blockNumber` — current block height
- `eth_getLogs` — ERC-20 `Transfer(address,address,uint256)` events
  filtered by USDC contract address and monitored deposit address

Built-in retry (5 attempts, 500ms backoff) via alloy's `RetryBackoffLayer`.
No request or connect timeouts — public RPC nodes can be slow, and the
retry layer handles transient failures.

---

## Conventions for Claude Code

- When creating a new module, start with the **trait definition and types**
  before writing implementation code.
- Use `tracing::info!` / `tracing::error!` for logging, not `println!`.
- All public functions and types need doc comments (`///`).
- Use `Result<T, AppError>` as the return type for all fallible operations.
- Prefer `?` operator for error propagation over manual `match` on Results.
- Write unit tests in the same file (`#[cfg(test)] mod tests { ... }`).
- Write integration tests in `src/tests/` using mock trait implementations.
- When adding a crate, add a comment in Cargo.toml explaining why it's there.
- Commit messages should reference which module is being worked on:
  `explorer: add retry logic` or `orchestrator: deposit check pass`.
