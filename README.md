# cryptosweep

Cron-based tool that monitors USDC deposits across 5 EVM chains, automatically
liquidates them on Coinbase (USDC → USD), and withdraws the proceeds via a linked Payment Method on Coinbase.com.

Runs every 15 minutes. A file lock prevents overlapping runs.

## Supported Chains

| Chain    | Chain ID | USDC Contract                              |
|----------|----------|--------------------------------------------|
| Ethereum | 1        | `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48` |
| Base     | 8453     | `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` |
| Arbitrum | 42161    | `0xaf88d065e77c8cC2239327C5EDb3A432268e5831` |
| Polygon  | 137      | `0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359` |
| Optimism | 10       | `0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85` |

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  CRON (every 15 min)                                         │
│                                                              │
│  1. Load config (~/.cryptosweep/config.toml)                 │
│  2. Set up Telegram notifier                                 │
│  3. Acquire lock (or notify + exit if locked)                │
│  4. Load state (~/.cryptosweep/state.json)                   │
│                                                              │
│  ┌──── DEPOSIT CHECK PASS (pipelined) ───────────────────┐   │
│  │                                                       │   │
│  │  Discovery (concurrent chain scans)                   │   │
│  │    → fetch block height + USDC transfers per chain    │   │
│  │    → filter: dust, dedup, confirmation depth          │   │
│  │    → send EligibleDeposit to channel ──────────┐      │   │
│  │                                                │      │   │
│  │                              mpsc channel (32) │      │   │
│  │                                                ▼      │   │
│  │  CbWorker (up to 5 concurrent jobs)                   │   │
│  │    → if amount > max_auto_amount → Failed, alert      │   │
│  │    → else: sell USDC → wait 30s → withdraw via PM     │   │
│  │    → exits when all senders drop                      │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  5. Prune records older than 90 days                         │
│  6. Persist state                                            │
│  7. Send Telegram alert if any errors occurred               │
│  8. Release lock, exit                                       │
└──────────────────────────────────────────────────────────────┘
```

Every run is idempotent. Deposit processing is gated on `processed_tx_hashes`,
and the block confirmation filter re-evaluates based on the current block
height.

## Module Structure

```
src/
├── main.rs               entrypoint: lock → load → check → persist → exit
├── config.rs             config loading from TOML
├── types.rs              shared types, traits, error definitions
├── explorer.rs           BlockExplorer trait + JSON-RPC impl (via alloy)
├── coinbase/
│   ├── mod.rs            CoinbaseClient trait + live implementation
│   ├── auth.rs           JWT ES256 signing (P-256 ECDSA)
│   └── models.rs         Coinbase API request/response types
├── notifier/
│   └── mod.rs            Notifier trait + Telegram implementation
├── cbworker.rs           CbWorker: channel-driven Coinbase sell + withdraw
├── orchestrator.rs       DepositCheck (pipelined discovery) + ChainDepositCheck
├── persistence.rs        state load/save, file locking, stale entry pruning
└── tests/
    ├── mod.rs
    ├── cassette.rs        HTTP response cassette framework
    ├── coinbase_mock.rs   mock implementations for testing
    └── fixtures/          recorded API responses
```

All modules expose async traits (`BlockExplorer`, `CoinbaseClient`, `Notifier`)
so implementations can be swapped for mocks in tests.

## Configuration

Copy the sample config and fill in your credentials:

```bash
mkdir -p ~/.cryptosweep
cp config.sample.toml ~/.cryptosweep/config.toml
```

### Config fields

| Field | Description |
|-------|-------------|
| `max_auto_amount` | Max USDC to auto-liquidate. Larger deposits are flagged as Failed. |
| `default_payment_method_id` | Coinbase payment method UUID |
| `deposit_addresses` | List of `{address, payment_method_id}` pairs to monitor |
| `coinbase_api_key` | CDP key name (`organizations/{org}/apiKeys/{key}`) |
| `coinbase_api_secret` | PEM-encoded ECDSA P-256 private key |
| `telegram_bot_token` | Telegram bot token for notifications |
| `telegram_chat_id` | Telegram chat ID for notifications |
| `confirmation_blocks` | Per-chain confirmation depth (default: 14) |
| `start_blocks` | Starting block per chain — required for initial scan |
| `coinbase_usdc_account_id` | Optional: skip account lookup pagination |
| `coinbase_usd_account_id` | Optional: skip account lookup pagination |

### Example

```toml
max_auto_amount = "10000.00"
default_payment_method_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

coinbase_api_key = "organizations/.../apiKeys/..."
coinbase_api_secret = "-----BEGIN EC PRIVATE KEY-----\n...\n-----END EC PRIVATE KEY-----"

telegram_bot_token = "123456789:XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
telegram_chat_id = "-1001234567890"

[[deposit_addresses]]
address = "0xYourCoinbaseDepositAddress"
payment_method_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[confirmation_blocks]
ethereum = 20
base = 14

[start_blocks]
ethereum = 22000000
base = 28000000
arbitrum = 300000000
polygon = 70000000
optimism = 135000000
```

## Requirements

- **Platform:** macOS or Linux (uses cron and POSIX file locking)
- **Rust:** edition 2024

Windows is not supported. WSL may work but is untested.

## Install

### Build from source

```bash
git clone <repo-url>
cd cryptosweep
cargo build --release
sudo ln -sf "$(pwd)/target/release/cryptosweep" /usr/local/bin/cryptosweep
```

### Set up config

```bash
mkdir -p ~/.cryptosweep
cp config.sample.toml ~/.cryptosweep/config.toml
# Edit config.toml with your credentials
```

### Set up log file

```bash
sudo touch /var/log/cryptosweep.log
sudo chown "$(whoami):$(whoami)" /var/log/cryptosweep.log
```

Optional: add logrotate to prevent unbounded growth:

```bash
sudo tee /etc/logrotate.d/cryptosweep <<'EOF'
/var/log/cryptosweep.log {
    weekly
    rotate 4
    compress
    missingok
    notifempty
}
EOF
```

### Set up cron

```bash
crontab -e
# Add:
*/15 * * * * /usr/local/bin/cryptosweep >> /var/log/cryptosweep.log 2>&1
```

### Run manually

```bash
cryptosweep
```

Safe to run at any time — the lock file prevents overlap with cron, and
`processed_tx_hashes` prevents double-processing.

## Safety

1. **No private keys** — the system never holds wallet keys. On-chain transfers are initiated manually via Ledger.
2. **Amount threshold** — deposits above `max_auto_amount` are recorded as Failed and trigger a Telegram alert.
3. **Deduplication** — `processed_tx_hashes` ensures a deposit is never liquidated twice, even across crashes.
4. **Full audit trail** — all operations logged via `tracing`; `BankPayment` records persisted to state file.

## Testing

```bash
cargo test
```

Smoke tests against live APIs (require credentials):

```bash
# Find Coinbase account IDs
cargo test coinbase_find_account_ids -- --ignored --nocapture

# List withdrawable payment methods
cargo test coinbase_list_withdrawable_payment_methods -- --ignored --nocapture

# Test Coinbase API auth
cargo test coinbase_api_auth_smoke -- --nocapture
```
