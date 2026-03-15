//! Block explorer: queries EVM nodes for USDC transfers and block heights.
//!
//! Uses direct JSON-RPC calls via the `alloy` crate to free public RPC
//! endpoints. No API keys required. Implements the `BlockExplorer` trait
//! from `types.rs`.

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::ClientBuilder;
use alloy::rpc::types::Filter;
use alloy::transports::layers::RetryBackoffLayer;
use chrono::Utc;
use rust_decimal::Decimal;
use tracing::info;

use crate::types::{AppError, BlockExplorer, Chain, UsdcReceive};

/// Maximum retry attempts on transient failures (429, 503).
const MAX_RETRIES: u32 = 5;

/// Initial backoff in milliseconds before the first retry.
const INITIAL_BACKOFF_MS: u64 = 500;

/// Compute units per second budget for retry rate limiting.
const COMPUTE_UNITS_PER_SEC: u64 = 330;

/// USDC uses 6 decimal places on all supported chains.
const USDC_DECIMALS: u32 = 6;

/// ERC-20 Transfer(address,address,uint256) event signature.
/// keccak256("Transfer(address,address,uint256)") — this is a constant
/// defined by the ERC-20 standard and will never change.
const TRANSFER_EVENT_TOPIC: FixedBytes<32> = FixedBytes([
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b,
    0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16,
    0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
]);

// ---------------------------------------------------------------------------
// RPC URL mapping
// ---------------------------------------------------------------------------

/// Return the public JSON-RPC endpoint URL for a chain.
/// These are free, no-API-key endpoints operated by chain foundations.
fn rpc_url(chain: Chain) -> &'static str {
    match chain {
        Chain::Ethereum => "https://ethereum.publicnode.com",
        Chain::Base     => "https://base.publicnode.com",
        Chain::Arbitrum => "https://arb1.arbitrum.io/rpc",
        Chain::Polygon  => "https://polygon-bor-rpc.publicnode.com",
        Chain::Optimism => "https://mainnet.optimism.io",
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a U256 atomic amount into a `Decimal`, dividing by 10^decimals.
/// Uses `u128` which is sufficient for USDC (6 decimals, supply < 60B).
fn u256_to_decimal(raw: U256, decimals: u32) -> Result<Decimal, AppError> {
    let raw_u128: u128 = raw.try_into()
        .map_err(|_| AppError::Explorer(format!("U256 value {raw} too large for u128")))?;
    Ok(Decimal::from(raw_u128) / Decimal::from(10u64.pow(decimals)))
}

/// Parse a hex address string (e.g. "0xA0b8...eB48") into an alloy `Address`.
fn parse_address(s: &str) -> Result<Address, AppError> {
    s.parse::<Address>()
        .map_err(|e| AppError::Explorer(format!("Bad address '{s}': {e}")))
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Block explorer backed by alloy's typed JSON-RPC provider, scoped to
/// a single chain. Each instance holds a pre-built provider with retry
/// and timeout configuration. Created once at bootstrap, reused across
/// all RPC calls for that chain.
pub struct LiveBlockExplorer {
    chain: Chain,
    usdc_contract: Address,
    provider: Box<dyn Provider>,
}

impl LiveBlockExplorer {
    /// Create a new explorer for the given chain.
    ///
    /// Builds an alloy HTTP provider with:
    /// - **Retries**: `RetryBackoffLayer` — up to 5 retries on 429/503,
    ///   500ms initial backoff.
    pub fn new(chain: Chain) -> Self {
        let url = rpc_url(chain).parse().expect("static RPC URL is valid");
        let usdc_contract = chain.usdc_contract()
            .parse::<Address>()
            .expect("static USDC contract address is valid");
        let rpc_client = ClientBuilder::default()
            .layer(RetryBackoffLayer::new(
                MAX_RETRIES,
                INITIAL_BACKOFF_MS,
                COMPUTE_UNITS_PER_SEC,
            ))
            .http(url);

        let provider = ProviderBuilder::new().connect_client(rpc_client);
        Self { chain, usdc_contract, provider: Box::new(provider) }
    }
}

impl BlockExplorer for LiveBlockExplorer {
    async fn get_block_height(&self) -> Result<u64, AppError> {
        let block = self.provider
            .get_block_number()
            .await
            .map_err(|e| AppError::Explorer(format!(
                "{}: block height failed: {e}", self.chain
            )))?;
        info!(chain = %self.chain, block, "Fetched block height");
        Ok(block)
    }

    async fn fetch_usdc_receives(
        &self,
        address: &str,
        since_block: u64,
    ) -> Result<Vec<UsdcReceive>, AppError> {
        let deposit_address = parse_address(address)?;

        // Filter for ERC-20 Transfer events to the deposit address.
        // topic0 = Transfer signature, topic1 = from (any), topic2 = to (deposit address).
        let filter = Filter::new()
            .address(self.usdc_contract)
            .from_block(since_block)
            .event_signature(TRANSFER_EVENT_TOPIC)
            .topic2(deposit_address);

        let logs = self.provider
            .get_logs(&filter)
            .await
            .map_err(|e| AppError::Explorer(format!(
                "{}: get_logs failed: {e}", self.chain
            )))?;
        let now = Utc::now();

        let receives: Vec<UsdcReceive> = logs
            .into_iter()
            .map(|log| {
                let block_number = log.block_number
                    .ok_or_else(|| AppError::Explorer("Log missing block_number".into()))?;
                let tx_hash = log.transaction_hash
                    .ok_or_else(|| AppError::Explorer("Log missing transaction_hash".into()))?;
                let raw_amount = U256::from_be_slice(log.data().data.as_ref());
                let amount = u256_to_decimal(raw_amount, USDC_DECIMALS)?;

                Ok(UsdcReceive {
                    tx_hash: format!("{tx_hash:#x}"),
                    amount,
                    block_number,
                    fetched_at: now,
                })
            })
            .collect::<Result<_, AppError>>()?;

        info!(
            chain = %self.chain,
            address,
            since_block,
            count = receives.len(),
            "Fetched USDC receives"
        );

        Ok(receives)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn u256_to_decimal_one_usdc() {
        // 1.0 USDC = 1_000_000 atomic units
        let raw = U256::from(1_000_000u64);
        let amount = u256_to_decimal(raw, 6).unwrap();
        assert_eq!(amount, dec!(1.0));
    }

    #[test]
    fn u256_to_decimal_large_amount() {
        // 2400.0 USDC = 2_400_000_000 atomic units
        let raw = U256::from(2_400_000_000u64);
        let amount = u256_to_decimal(raw, 6).unwrap();
        assert_eq!(amount, dec!(2400.0));
    }

    #[test]
    fn u256_to_decimal_zero() {
        let raw = U256::ZERO;
        let amount = u256_to_decimal(raw, 6).unwrap();
        assert_eq!(amount, dec!(0));
    }

    #[test]
    fn parse_address_valid() {
        let addr = parse_address("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        assert_eq!(format!("{addr:#x}"), "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    }

    #[test]
    fn parse_address_invalid() {
        assert!(parse_address("not_an_address").is_err());
    }
}
