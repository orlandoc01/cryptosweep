//! Shared types, error definitions, traits, and constants for the cryptosweep project.
//!
//! This module is the "contract" between all other modules. Every struct, enum,
//! and trait used across module boundaries lives here. Keeping them in one place
//! avoids circular dependencies and makes the data model easy to audit.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Unified error type. Each module has its own variant so the orchestrator
/// can pattern-match on error sources.
///
/// We use `thiserror` to derive `std::error::Error` — it generates the
/// Display impl from the `#[error("...")]` attributes and the `From` impls
/// from `#[from]`, reducing boilerplate while keeping errors explicit.
#[derive(Error, Debug)]
pub enum AppError {
    #[error("Block explorer error: {0}")]
    Explorer(String),

    #[error("Coinbase API error: {0}")]
    Coinbase(String),

    #[error("Notification delivery failed: {0}")]
    Notification(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Chain
// ---------------------------------------------------------------------------

/// A dedicated Coinbase deposit address mapped to a single PM destination.
/// Generate each address in the Coinbase UI under "Receive" and use it
/// exclusively for cryptosweep — do not reuse for other trading activity.
#[derive(Debug, Clone, Serialize, Deserialize, garde::Validate)]
#[garde(allow_unvalidated)]
pub struct DepositAddressConfig {
    /// The on-chain address to monitor across all supported chains.
    #[garde(custom(validate_eth_address))]
    pub address: String,
    /// Coinbase payment method ID for the destination bank account.
    pub payment_method_id: String,
}

/// Validate that a string is a 42-character hex Ethereum address.
fn validate_eth_address(value: &str, _ctx: &()) -> garde::Result {
    if value.len() == 42
        && value.starts_with("0x")
        && value[2..].chars().all(|c| c.is_ascii_hexdigit())
    {
        return Ok(());
    }
    Err(garde::Error::new(
        "must be a 42-character hex address starting with 0x",
    ))
}

/// Supported chains for USDC deposit monitoring.
/// All chains are queried via block explorer APIs using a single API key —
/// the chain is selected by passing the appropriate chain ID parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Display, EnumIter)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Chain {
    Ethereum,
    Base,
    Arbitrum,
    Polygon,
    Optimism,
}

impl Chain {
    /// The native USDC contract address on this chain.
    /// Passed as the `contractaddress` parameter in block explorer requests.
    pub fn usdc_contract(&self) -> &'static str {
        match self {
            Chain::Ethereum => "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            Chain::Base     => "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            Chain::Arbitrum => "0xaf88d065e77c8cC2239327C5EDb3A432268e5831",
            Chain::Polygon  => "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359",
            Chain::Optimism => "0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85",
        }
    }
}

// ---------------------------------------------------------------------------
// BankPayment + BankPaymentOutcome
// ---------------------------------------------------------------------------

/// Audit record for a single USDC deposit detection and Coinbase liquidation.
/// Created when a sufficiently aged deposit is processed (successfully or not).
/// No state machine — the flow (sell -> withdraw) executes atomically in one
/// pass. This record exists solely for auditability.
///
/// ## Key format
/// `{YYYYMMDD}-{tx_hash_prefix}`
/// e.g., `"20260328-0xabc123"`
/// Date is the UTC date the deposit was processed. tx_hash_prefix is the
/// first 8 characters of the on-chain transaction hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BankPayment {
    /// Human-readable key (also the `BTreeMap` key in `PersistedState`).
    /// Derived from `processed_at` + `deposit_tx_hash` — immutable after
    /// construction to preserve the invariant.
    pub id: String,

    /// The deposit address that received the USDC.
    pub deposit_address: String,

    /// The chain on which the deposit was detected.
    pub chain: Chain,

    /// On-chain transaction hash of the inbound USDC transfer.
    pub deposit_tx_hash: String,

    /// USDC amount received.
    pub usdc_amount: Decimal,

    /// Coinbase payment method ID for the destination.
    pub payment_method_id: String,

    /// Outcome of the sell + withdraw attempt.
    pub outcome: BankPaymentOutcome,

    /// When the deposit was first seen by the block explorer query.
    pub deposit_timestamp: DateTime<Utc>,

    /// When cryptosweep processed this deposit (wall clock time).
    pub processed_at: DateTime<Utc>,
}

impl BankPayment {
    /// Derives the human-readable `BankPayment` ID.
    /// Format: `{YYYYMMDD}-{tx_hash_prefix}` (first 8 chars of tx hash).
    pub fn make_id(processed_at: &DateTime<Utc>, tx_hash: &str) -> String {
        let prefix = &tx_hash[..8.min(tx_hash.len())];
        format!("{}-{}", processed_at.format("%Y%m%d"), prefix)
    }
}

/// The result of the sell + withdraw attempt for a processed deposit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BankPaymentOutcome {
    /// Sell executed and withdrawal initiated successfully.
    Initiated {
        sell_order_id: String,
        usd_proceeds: Decimal,
        withdrawal_transfer_id: String,
        initiated_at: DateTime<Utc>,
    },
    /// Amount exceeded max_auto_amount, or a permanent API error occurred.
    /// Requires manual intervention.
    Failed {
        error: String,
        failed_at: DateTime<Utc>,
    },
}

// ---------------------------------------------------------------------------
// LastSeenBlock
// ---------------------------------------------------------------------------

/// Block height and timestamp of the last successful block explorer poll
/// for a given chain. The `updated_at` field enables staleness-based
/// alerting: explorer errors are only escalated to Telegram when the
/// last successful data for the chain is older than 6 hours.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastSeenBlock {
    pub block_number: u64,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// PersistedState
// ---------------------------------------------------------------------------

/// Top-level state file. Serialized to `~/.cryptosweep/state.json` on every run.
/// BTreeMap/BTreeSet are used for all collections so the JSON file stays in
/// lexicographic (and given date-prefixed keys, chronological) order —
/// making the state file easy to audit without tooling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PersistedState {
    /// All bank payment audit records, keyed by `BankPayment.id`.
    /// Retained indefinitely for audit history.
    pub bank_payments: BTreeMap<String, BankPayment>,

    /// On-chain tx hashes that have been fully processed (liquidated or
    /// permanently failed). Primary deduplication guard — a hash here is
    /// never re-processed regardless of what block explorer returns.
    pub processed_tx_hashes: BTreeSet<String>,

    /// Latest block height and poll timestamp per chain.
    /// Updated on every successful poll. The `updated_at` timestamp
    /// enables staleness-based alerting for transient explorer errors.
    /// Used as `since_block` on subsequent block explorer queries to avoid
    /// rescanning full history. On first scan this key is absent — the
    /// orchestrator falls back to `start_blocks[chain]` from config.
    pub last_seen_block: BTreeMap<Chain, LastSeenBlock>,
}

// ---------------------------------------------------------------------------
// Pipeline types (chain discovery → cbworker)
// ---------------------------------------------------------------------------

/// A confirmed, non-dust, non-duplicate USDC deposit ready for Coinbase
/// liquidation. Produced by chain discovery, consumed by `CbWorker`.
pub struct EligibleDeposit {
    /// The on-chain address that received the deposit.
    pub deposit_address: String,
    /// The chain on which the deposit was detected.
    pub chain: Chain,
    /// On-chain transaction hash.
    pub tx_hash: String,
    /// USDC amount received.
    pub usdc_amount: Decimal,
    /// Coinbase payment method ID for the destination bank account.
    pub payment_method_id: String,
    /// When the deposit was first seen by the block explorer query.
    pub deposit_timestamp: DateTime<Utc>,
    /// When cryptosweep processed this deposit.
    pub processed_at: DateTime<Utc>,
}

/// Result of processing one `EligibleDeposit` through the Coinbase
/// sell + withdraw pipeline.
pub struct LiquidationResult {
    /// The audit record for this deposit.
    pub payment: BankPayment,
    /// On-chain tx hash (for adding to the dedup set).
    pub tx_hash: String,
}

// ---------------------------------------------------------------------------
// Coinbase response types
// ---------------------------------------------------------------------------

/// Result of selling USDC for USD on Coinbase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SellResult {
    /// Coinbase order ID for the sell.
    pub order_id: String,
    /// USDC amount that was sold.
    pub usdc_sold: Decimal,
    /// USD amount received after conversion.
    pub usd_received: Decimal,
    /// When the sell was executed.
    pub executed_at: DateTime<Utc>,
}

/// Result of initiating a withdrawal from Coinbase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WithdrawalResult {
    /// Coinbase withdrawal transfer UUID.
    pub withdrawal_transfer_id: String,
    /// USD amount withdrawn.
    pub usd_amount: Decimal,
    /// When the withdrawal was initiated.
    pub initiated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Block explorer types
// ---------------------------------------------------------------------------

/// A USDC ERC-20 transfer received by a monitored deposit address,
/// as returned by a block explorer query.
#[derive(Debug, Clone)]
pub struct UsdcReceive {
    /// On-chain transaction hash.
    pub tx_hash: String,
    /// USDC amount received.
    pub amount: Decimal,
    /// Block number the transfer was included in.
    pub block_number: u64,
    /// Wall-clock time when this receive was fetched from the RPC node.
    /// Not the on-chain block timestamp (that would require an extra RPC call).
    pub fetched_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Module traits
// ---------------------------------------------------------------------------

/// Block explorer client scoped to a single chain.
/// Each instance holds a pre-built provider for one chain.
/// The orchestrator holds a map of `Chain -> impl BlockExplorer`.
pub trait BlockExplorer: Send + Sync {
    /// Returns the current block height.
    /// Used with `confirmation_blocks` to determine which receives are
    /// finalized: a receive is eligible when
    /// `current_block - receive.block_number >= confirmation_blocks`.
    async fn get_block_height(&self) -> Result<u64, AppError>;

    /// Returns all USDC ERC-20 receives to `address` with block number
    /// greater than `since_block`. Returns an empty vec if none found.
    /// The caller is responsible for confirmation filtering and deduplication.
    async fn fetch_usdc_receives(
        &self,
        address: &str,
        since_block: u64,
    ) -> Result<Vec<UsdcReceive>, AppError>;
}

/// Coinbase client: exchange-side operations.
pub trait CoinbaseClient: Send + Sync {
    /// Sell USDC for USD. Returns order ID and USD proceeds.
    async fn sell_usdc(&self, amount: Decimal) -> Result<SellResult, AppError>;

    /// Initiate withdrawal to the specified payment method.
    /// Returns the Coinbase withdrawal transfer ID.
    async fn withdraw_usd(
        &self,
        amount: Decimal,
        payment_method_id: &str,
    ) -> Result<WithdrawalResult, AppError>;
}

/// Notifier: one-way alerts to the user via Telegram.
pub trait Notifier: Send + Sync {
    /// Send a generic message (errors, alerts, etc.)
    async fn notify(&self, message: &str) -> Result<(), AppError>;
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use strum::IntoEnumIterator;

    // -- Chain tests --

    #[test]
    fn chain_iter_returns_five() {
        assert_eq!(Chain::iter().count(), 5);
    }

    #[test]
    fn chain_display_is_lowercase() {
        assert_eq!(Chain::Ethereum.to_string(), "ethereum");
        assert_eq!(Chain::Base.to_string(), "base");
        assert_eq!(Chain::Arbitrum.to_string(), "arbitrum");
        assert_eq!(Chain::Polygon.to_string(), "polygon");
        assert_eq!(Chain::Optimism.to_string(), "optimism");
    }

    #[test]
    fn chain_usdc_contracts_are_checksummed() {
        // All USDC contracts should start with 0x and be 42 chars
        for chain in Chain::iter() {
            let addr = chain.usdc_contract();
            assert!(addr.starts_with("0x"), "{chain}: {addr}");
            assert_eq!(addr.len(), 42, "{chain}: {addr}");
        }
    }

    // -- BankPayment::make_id tests --

    #[test]
    fn bank_payment_make_id_derives_correctly() {
        let now = Utc::now();
        let id = BankPayment::make_id(&now, "0xabc123def456");
        let expected_date = now.format("%Y%m%d").to_string();
        assert_eq!(id, format!("{}-0xabc123", expected_date));
    }

    #[test]
    fn bank_payment_make_id_short_tx_hash() {
        let now = Utc::now();
        let id = BankPayment::make_id(&now, "0xab");
        let expected_date = now.format("%Y%m%d").to_string();
        assert_eq!(id, format!("{}-0xab", expected_date));
    }

    // -- PersistedState serde round-trip --

    #[test]
    fn persisted_state_serde_round_trip() {
        let mut state = PersistedState::default();
        let now = Utc::now();

        let tx_hash = "0xabc123def456".to_string();
        let payment = BankPayment {
            id: BankPayment::make_id(&now, &tx_hash),
            deposit_address: "0xDeposit".to_string(),
            chain: Chain::Base,
            deposit_tx_hash: tx_hash,
            usdc_amount: dec!(2400.00),
            payment_method_id: "a1b2c3d4-uuid".to_string(),
            outcome: BankPaymentOutcome::Initiated {
                sell_order_id: "order-1".to_string(),
                usd_proceeds: dec!(2399.50),
                withdrawal_transfer_id: "withdraw-1".to_string(),
                initiated_at: now,
            },
            deposit_timestamp: now,
            processed_at: now,
        };
        state.bank_payments.insert(payment.id.clone(), payment);

        state.processed_tx_hashes.insert("0xabc123def456".to_string());
        state.last_seen_block.insert(Chain::Base, LastSeenBlock {
            block_number: 12345678,
            updated_at: now,
        });

        // Serialize -> deserialize round-trip
        let json = serde_json::to_string_pretty(&state).expect("serialize");
        let restored: PersistedState = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored, state);
    }

    #[test]
    fn persisted_state_default_is_empty() {
        let state = PersistedState::default();
        assert!(state.bank_payments.is_empty());
        assert!(state.processed_tx_hashes.is_empty());
        assert!(state.last_seen_block.is_empty());
    }
}
