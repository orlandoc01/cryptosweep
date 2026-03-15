//! Configuration loading from `~/.cryptosweep/config.toml`.
//!
//! The `CryptoSweepConfig` struct mirrors the TOML file structure. Fields are
//! documented with their purpose and defaults where applicable.

use std::collections::HashMap;
use std::path::PathBuf;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::{Chain, DepositAddressConfig, AppError};

/// Fallback confirmation depth used when a chain is not present in
/// config.confirmation_blocks.
pub const DEFAULT_CONFIRMATION_BLOCKS: u64 = 14;


/// Loaded from `~/.cryptosweep/config.toml`.
/// Cron scheduling lives outside the application (crontab).
#[derive(Debug, Clone, Serialize, Deserialize, garde::Validate)]
#[garde(allow_unvalidated)]
pub struct CryptoSweepConfig {
    /// Maximum USDC deposit amount the system will process automatically.
    /// Deposits above this threshold are logged as Failed and the user is
    /// notified. Catches parsing errors and data anomalies.
    pub max_auto_amount: Decimal,

    /// Default Coinbase payment method ID used when a deposit address
    /// does not specify one.
    pub default_payment_method_id: String,

    /// Deposit addresses to monitor, each mapped to a Coinbase payment
    /// method ID. Generate each address in Coinbase as a dedicated address
    /// used only by cryptosweep — one per payment destination.
    /// All supported chains are polled for each address.
    #[serde(default)]
    #[garde(length(min = 1), dive)]
    pub deposit_addresses: Vec<DepositAddressConfig>,

    /// Minimum number of block confirmations a USDC receive must have
    /// before it is eligible for liquidation. A receive is considered
    /// finalized when `current_block - receive_block >= confirmation_blocks`.
    /// Falls back to DEFAULT_CONFIRMATION_BLOCKS for unconfigured chains.
    #[serde(default)]
    pub confirmation_blocks: HashMap<Chain, u64>,

    /// Starting block number per chain, used when the persisted state has
    /// no `last_seen_block` for a given (chain, address) pair. An entry
    /// should be provided for every chain that will be scanned — the
    /// operator sets these to a recent block number at deployment time so
    /// the first scan doesn't start from genesis. Chains without an entry
    /// are skipped during the deposit check pass (with a warning).
    #[serde(default)]
    pub start_blocks: HashMap<Chain, u64>,

    /// Coinbase API key name (`organizations/{org_id}/apiKeys/{key_id}`).
    pub coinbase_api_key: String,

    /// Coinbase API secret — PKCS8 PEM-encoded ECDSA P-256 private key.
    pub coinbase_api_secret: String,

    /// Telegram bot token for notifications.
    pub telegram_bot_token: String,

    /// Telegram chat ID for notifications.
    pub telegram_chat_id: String,

    /// Coinbase USDC account UUID. Avoids paginating through all accounts.
    /// Find with: `cargo test coinbase_find_account_ids -- --ignored --nocapture`
    #[serde(default)]
    pub coinbase_usdc_account_id: Option<String>,

    /// Coinbase USD account UUID. Avoids paginating through all accounts.
    #[serde(default)]
    pub coinbase_usd_account_id: Option<String>,

    /// Seconds to wait after selling USDC before initiating withdrawal.
    /// Gives the conversion time to settle in the USD account.
    /// Defaults to 30 seconds.
    #[serde(default = "default_sell_settle_delay_secs")]
    pub sell_settle_delay_secs: u64,

    /// Path to state file (default: `~/.cryptosweep/state.json`).
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
}

fn default_sell_settle_delay_secs() -> u64 {
    30
}

fn default_state_file() -> PathBuf {
    let home = std::env::var("HOME")
        .expect("HOME environment variable must be set (no default state_file path without it)");
    PathBuf::from(home).join(".cryptosweep").join("state.json")
}

impl CryptoSweepConfig {
    /// Load configuration from a TOML file at the given path.
    ///
    /// Returns `AppError::Config` if the file can't be read or parsed.
    /// Missing optional fields use their `#[serde(default)]` values.
    pub fn load(path: &std::path::Path) -> Result<Self, AppError> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            AppError::Config(format!("Failed to read config file {:?}: {}", path, e))
        })?;
        let config: Self = toml::from_str(&contents).map_err(|e| {
            AppError::Config(format!("Failed to parse config file {:?}: {}", path, e))
        })?;
        garde::Validate::validate(&config).map_err(|report| {
            AppError::Config(format!("Invalid config: {report}"))
        })?;
        Ok(config)
    }

    /// Returns the required confirmation depth (in blocks) for a given chain.
    /// Falls back to DEFAULT_CONFIRMATION_BLOCKS if not configured.
    pub fn confirmation_blocks_for(&self, chain: Chain) -> u64 {
        self.confirmation_blocks
            .get(&chain)
            .copied()
            .unwrap_or(DEFAULT_CONFIRMATION_BLOCKS)
    }

    /// Returns the configured starting block number for a given chain.
    /// Used as the `since_block` on the first scan when no persisted
    /// `last_seen_block` exists yet.
    ///
    /// Returns `None` if the chain is not configured — the caller should
    /// skip scanning that chain.
    pub fn start_block_for(&self, chain: Chain) -> Option<u64> {
        self.start_blocks.get(&chain).copied()
    }
}

/// Default path to the config file.
pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME")
        .expect("HOME environment variable must be set (no default config path without it)");
    PathBuf::from(home).join(".cryptosweep").join("config.toml")
}

/// Load config for tests.
///
/// Tries the production config at `~/.cryptosweep/config.toml` first so
/// that `#[ignore]` tests that hit real APIs use real credentials. Falls
/// back to `config.sample.toml` in the project root (placeholder values
/// are fine for cassette-replay tests).
#[cfg(test)]
pub fn load_test_config() -> CryptoSweepConfig {
    let prod_path = default_config_path();
    if prod_path.exists() {
        return CryptoSweepConfig::load(&prod_path)
            .expect("~/.cryptosweep/config.toml exists but failed to parse");
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR should be set during cargo test");
    let path = std::path::PathBuf::from(manifest_dir).join("config.sample.toml");
    CryptoSweepConfig::load(&path)
        .expect("config.sample.toml should exist and parse in project root")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal required TOML snippet reused across config tests.
    const BASE_TOML: &str = r#"
max_auto_amount = "10000.00"
default_payment_method_id = "pm-default-123"
coinbase_api_key = "organizations/org1/apiKeys/key1"
coinbase_api_secret = "-----BEGIN PRIVATE KEY-----\ntest\n-----END PRIVATE KEY-----"
telegram_bot_token = "123456:ABC-DEF"
telegram_chat_id = "987654321"

[start_blocks]
ethereum = 22000000
base = 28000000
arbitrum = 300000000
polygon = 70000000
optimism = 135000000
"#;

    #[test]
    fn parse_config_from_toml() {
        let config: CryptoSweepConfig = toml::from_str(BASE_TOML).expect("should parse");

        assert_eq!(config.max_auto_amount, Decimal::new(10000_00, 2));
        assert_eq!(config.default_payment_method_id, "pm-default-123");
        assert_eq!(config.telegram_chat_id, "987654321");
    }

    /// Start blocks section appended to test TOMLs that add extra sections.
    const START_BLOCKS_TOML: &str = r#"
[start_blocks]
ethereum = 22000000
base = 28000000
arbitrum = 300000000
polygon = 70000000
optimism = 135000000
"#;

    #[test]
    fn confirmation_blocks_defaults() {
        let toml_str = format!(
            r#"
max_auto_amount = "10000.00"
default_payment_method_id = "pm-default"
coinbase_api_key = "key"
coinbase_api_secret = "secret"
telegram_bot_token = "bot"
telegram_chat_id = "chat"

[confirmation_blocks]
base = 20
{START_BLOCKS_TOML}"#
        );
        let config: CryptoSweepConfig = toml::from_str(&toml_str).expect("should parse");

        assert_eq!(config.confirmation_blocks_for(Chain::Base), 20);
        assert_eq!(
            config.confirmation_blocks_for(Chain::Ethereum),
            DEFAULT_CONFIRMATION_BLOCKS
        );
    }

    #[test]
    fn start_blocks_required_and_resolved() {
        let config: CryptoSweepConfig = toml::from_str(BASE_TOML).expect("should parse");

        assert_eq!(config.start_block_for(Chain::Ethereum), Some(22_000_000));
        assert_eq!(config.start_block_for(Chain::Base), Some(28_000_000));
        assert_eq!(config.start_block_for(Chain::Arbitrum), Some(300_000_000));
    }

    #[test]
    fn start_blocks_missing_chain_returns_none() {
        let toml_str = r#"
max_auto_amount = "10000.00"
default_payment_method_id = "pm-default"
coinbase_api_key = "key"
coinbase_api_secret = "secret"
telegram_bot_token = "bot"
telegram_chat_id = "chat"

[start_blocks]
ethereum = 22000000
"#;
        let config: CryptoSweepConfig = toml::from_str(toml_str).expect("should parse");

        assert_eq!(config.start_block_for(Chain::Ethereum), Some(22_000_000));
        assert_eq!(config.start_block_for(Chain::Base), None);
    }

    #[test]
    fn load_missing_file_returns_error() {
        let result = CryptoSweepConfig::load(std::path::Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    /// Helper: write a TOML string to a temp file and load it.
    fn load_from_str(toml: &str) -> Result<CryptoSweepConfig, AppError> {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        CryptoSweepConfig::load(&path)
    }

    #[test]
    fn load_rejects_empty_deposit_addresses() {
        let toml_str = r#"
max_auto_amount = "10000.00"
default_payment_method_id = "pm-default"
coinbase_api_key = "key"
coinbase_api_secret = "secret"
telegram_bot_token = "bot"
telegram_chat_id = "chat"

[start_blocks]
ethereum = 22000000
"#;
        let err = load_from_str(toml_str).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
        assert!(err.to_string().contains("deposit_addresses"));
    }

    #[test]
    fn load_rejects_invalid_deposit_address() {
        let toml_str = r#"
max_auto_amount = "10000.00"
default_payment_method_id = "pm-default"
coinbase_api_key = "key"
coinbase_api_secret = "secret"
telegram_bot_token = "bot"
telegram_chat_id = "chat"

[[deposit_addresses]]
address = "not-a-hex-address"
payment_method_id = "pm-1"

[start_blocks]
ethereum = 22000000
"#;
        let err = load_from_str(toml_str).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
        assert!(err.to_string().contains("deposit_addresses"));
    }

    #[test]
    fn load_accepts_valid_deposit_address() {
        let toml_str = r#"
max_auto_amount = "10000.00"
default_payment_method_id = "pm-default"
coinbase_api_key = "key"
coinbase_api_secret = "secret"
telegram_bot_token = "bot"
telegram_chat_id = "chat"

[[deposit_addresses]]
address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
payment_method_id = "pm-1"

[start_blocks]
ethereum = 22000000
"#;
        let config = load_from_str(toml_str).expect("valid config should load");
        assert_eq!(config.deposit_addresses.len(), 1);
    }
}
