//! Orchestrator: deposit check pass.
//!
//! Polls block explorers for USDC deposits and sends eligible ones to
//! the `CbWorker` via channel. Chain discovery and Coinbase execution
//! run concurrently via `tokio::join!` — the first deposit starts
//! liquidation while other chains are still scanning.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use futures::future::join_all;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strum::IntoEnumIterator;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::cbworker::CbWorker;
use crate::config::CryptoSweepConfig;
use crate::types::{
    BlockExplorer, Chain, CoinbaseClient, EligibleDeposit, LastSeenBlock, Notifier,
    PersistedState, UsdcReceive,
};

/// Minimum USDC amount to process. Deposits below this are ignored as dust.
const USDC_DUST_THRESHOLD: Decimal = dec!(1.0);

/// How long a chain can go without a successful block explorer response
/// before errors are escalated to Telegram. At 30-minute cron intervals,
/// 6 hours ≈ 12 consecutive failures.
const CHAIN_STALE_THRESHOLD_HOURS: i64 = 6;

/// Bounded channel capacity for the discovery → worker pipeline.
const CHANNEL_CAPACITY: usize = 32;

// ---------------------------------------------------------------------------
// Deposit check pass
// ---------------------------------------------------------------------------

/// Orchestrator for a single deposit check pass.
///
/// Holds references to all dependencies needed during one cron invocation.
/// Constructed in `main`, consumed by `run()`, then dropped.
///
/// Polls block explorers for USDC deposits and sends eligible ones to
/// the `CbWorker` for Coinbase sell + withdraw. All chain scans run
/// concurrently, pipelined with the worker.
pub struct DepositCheck<'a, E: BlockExplorer, C: CoinbaseClient, N: Notifier> {
    config: &'a CryptoSweepConfig,
    state: &'a mut PersistedState,
    explorers: &'a HashMap<Chain, E>,
    coinbase: &'a C,
    notifier: &'a N,
}

impl<'a, E: BlockExplorer, C: CoinbaseClient, N: Notifier> DepositCheck<'a, E, C, N> {
    /// Create a new deposit check pass with the given dependencies.
    pub fn new(
        config: &'a CryptoSweepConfig,
        state: &'a mut PersistedState,
        explorers: &'a HashMap<Chain, E>,
        coinbase: &'a C,
        notifier: &'a N,
    ) -> Self {
        Self { config, state, explorers, coinbase, notifier }
    }

    /// Run the deposit check pass: chain discovery pipelined with Coinbase
    /// execution via `tokio::join!`. Returns a list of non-fatal errors
    /// (e.g. RPC failures on individual chains) so the caller can report
    /// them via Telegram.
    pub async fn run(self) -> Vec<String> {
        let DepositCheck { config, state, explorers, coinbase, notifier } = self;

        // Shared immutable borrow of state for concurrent reads (dedup
        // checks, last_seen_block lookups). Mutable access resumes after
        // both futures complete.
        let state_ref: &PersistedState = state;

        let (tx, rx) = mpsc::channel::<EligibleDeposit>(CHANNEL_CAPACITY);

        // Discovery: chain scans send eligible deposits to channel immediately.
        let discovery_fut = async {
            let futures: Vec<_> = Chain::iter()
                .filter_map(|chain| {
                    let explorer = explorers.get(&chain)?;
                    let check = ChainDepositCheck {
                        config,
                        state: state_ref,
                        explorer,
                        chain,
                        tx: tx.clone(),
                    };
                    Some(async move { (chain, check.run().await) })
                })
                .collect();

            // Drop the original sender — only clones inside futures remain.
            // When all futures complete, their senders drop, closing the channel.
            drop(tx);
            join_all(futures).await
        };

        // Execution: worker processes deposits as they arrive.
        let worker = CbWorker {
            coinbase,
            notifier,
            max_auto_amount: config.max_auto_amount,
            sell_settle_delay_secs: config.sell_settle_delay_secs,
        };
        let liquidate_fut = worker.run(rx);

        // Run both concurrently — worker starts processing while chains scan.
        let (chain_results, liquidation_results) = tokio::join!(
            discovery_fut, liquidate_fut
        );

        // Merge discovery metadata into state.
        let mut errors = Vec::new();
        for (chain, result) in chain_results {
            if let Some(block) = result.latest_seen_block() {
                state.last_seen_block.insert(chain, LastSeenBlock {
                    block_number: block,
                    updated_at: Utc::now(),
                });
            }
            for hash in result.processed_tx_hashes {
                state.processed_tx_hashes.insert(hash);
            }
            errors.extend(result.errors);
        }

        // Merge execution results into state.
        for liquidation in liquidation_results {
            state.bank_payments.insert(liquidation.payment.id.clone(), liquidation.payment);
            state.processed_tx_hashes.insert(liquidation.tx_hash);
        }

        info!("Deposit check pass complete");
        errors
    }
}

// ---------------------------------------------------------------------------
// Per-chain deposit check
// ---------------------------------------------------------------------------

/// Results collected from scanning deposits on a single chain.
///
/// Forms a monoid with `ChainResult::EMPTY` as identity and `Add` as the
/// combining operation. Each field combines independently: vectors
/// concatenate, and `confirmed_frontier` takes `min` (with `Some(u64::MAX)`
/// as identity, `None` absorbing).
struct ChainResult {
    /// Tx hashes sent to the worker (for the dedup set).
    processed_tx_hashes: Vec<String>,
    /// Internal confirmed frontier. `Some(u64::MAX)` is the monoid identity
    /// for `min` — use `latest_seen_block()` to access the real value.
    confirmed_frontier: Option<u64>,
    /// Non-fatal errors encountered during this chain's scan.
    errors: Vec<String>,
}

impl ChainResult {
    /// Monoid identity.
    const EMPTY: Self = Self {
        processed_tx_hashes: Vec::new(),
        confirmed_frontier: Some(u64::MAX),
        errors: Vec::new(),
    };

    /// Returns the confirmed frontier block number, filtering out the
    /// `u64::MAX` monoid identity so it never leaks into persisted state.
    fn latest_seen_block(&self) -> Option<u64> {
        self.confirmed_frontier.filter(|&b| b != u64::MAX)
    }
}

impl std::iter::Sum for ChainResult {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::EMPTY, |acc, r| acc + r)
    }
}

impl std::ops::Add for ChainResult {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self {
        self.processed_tx_hashes.extend(rhs.processed_tx_hashes);
        self.confirmed_frontier = self.confirmed_frontier
            .and_then(|a| rhs.confirmed_frontier.map(|b| a.min(b)));
        self.errors.extend(rhs.errors);
        self
    }
}

/// Per-chain orchestrator that holds resolved chain-level context.
///
/// Created by `DepositCheck::run` for each chain. Resolves the block height
/// and scan window, runs all deposit address scans, and sends eligible
/// deposits to the `CbWorker` via channel. Returns a `ChainResult` with
/// state updates to merge.
struct ChainDepositCheck<'a, E: BlockExplorer> {
    config: &'a CryptoSweepConfig,
    state: &'a PersistedState,
    explorer: &'a E,
    chain: Chain,
    tx: mpsc::Sender<EligibleDeposit>,
}

impl<E: BlockExplorer> ChainDepositCheck<'_, E> {
    /// Returns `true` if the last successful block explorer poll for this
    /// chain is older than `CHAIN_STALE_THRESHOLD_HOURS`, or if no data
    /// has ever been recorded (treat as stale to alert immediately).
    fn is_chain_stale(&self) -> bool {
        self.state.last_seen_block.get(&self.chain)
            .map(|lsb| Utc::now() - lsb.updated_at > chrono::Duration::hours(CHAIN_STALE_THRESHOLD_HOURS))
            .unwrap_or(true)
    }

    /// Run the per-chain deposit check: fetch the current block height,
    /// resolve the scan window, scan all deposit addresses, then return
    /// results for the caller to merge into state.
    async fn run(self) -> ChainResult {
        let current_block = match self.explorer.get_block_height().await {
            Ok(b) => b,
            Err(e) if self.is_chain_stale() => {
                error!(chain = %self.chain, %e, "Failed to get block height (stale — escalating)");
                return ChainResult {
                    confirmed_frontier: None,
                    errors: vec![format!("{}: block height failed: {e}", self.chain)],
                    ..ChainResult::EMPTY
                };
            }
            Err(e) => {
                warn!(chain = %self.chain, %e, "Failed to get block height (recent data exists — suppressing)");
                return ChainResult {
                    confirmed_frontier: None,
                    ..ChainResult::EMPTY
                };
            }
        };

        // Use persisted last_seen_block, or fall back to the configured
        // start_block for this chain. If neither exists, skip — the
        // operator must configure start_blocks for every chain.
        let option_since_block = self.state.last_seen_block.get(&self.chain)
            .map(|lsb| lsb.block_number)
            .or_else(|| self.config.start_block_for(self.chain));
        let Some(since_block) = option_since_block else {
            warn!(
                chain = %self.chain,
                "No last_seen_block in state and no start_block in config, skipping"
            );
            return ChainResult {
                confirmed_frontier: None,
                ..ChainResult::EMPTY
            };
        };

        let scans: Vec<_> = self.config.deposit_addresses.iter()
            .map(|dc| self.address_scan(&dc.address, &dc.payment_method_id, since_block, current_block))
            .collect();

        join_all(scans).await.into_iter().sum()
    }

    /// Fetch USDC receives for a single address and send each eligible
    /// deposit to the worker channel.
    ///
    /// Returns a `ChainResult` with `confirmed_frontier: None` when
    /// `fetch_usdc_receives` fails.
    async fn address_scan(
        &self,
        address: &str,
        payment_method_id: &str,
        since_block: u64,
        current_block: u64,
    ) -> ChainResult {
        let receives = match self.explorer
            .fetch_usdc_receives(address, since_block)
            .await
        {
            Ok(r) => r,
            Err(e) if self.is_chain_stale() => {
                error!(chain = %self.chain, address, %e, "Failed to fetch USDC receives (stale — escalating)");
                return ChainResult {
                    confirmed_frontier: None,
                    errors: vec![format!("{}: fetch receives failed for {address}: {e}", self.chain)],
                    ..ChainResult::EMPTY
                };
            }
            Err(e) => {
                warn!(chain = %self.chain, address, %e, "Failed to fetch USDC receives (recent data exists — suppressing)");
                return ChainResult {
                    confirmed_frontier: None,
                    ..ChainResult::EMPTY
                };
            }
        };

        let mut result = ChainResult::EMPTY;
        let mut scan_dedup: HashSet<String> = HashSet::new();

        for receive in &receives {
            if self.send_if_eligible(
                address, payment_method_id, current_block, receive, &scan_dedup,
            ).await {
                result.processed_tx_hashes.push(receive.tx_hash.clone());
                scan_dedup.insert(receive.tx_hash.clone());
            }
        }

        // Successful fetch — advance the confirmed frontier for this scan.
        result.confirmed_frontier = Some(current_block.saturating_sub(
            self.config.confirmation_blocks_for(self.chain),
        ));

        result
    }

    /// Evaluate a single USDC receive: filter dust/duplicates/unconfirmed.
    /// If eligible, send to the worker channel. Returns `true` if the
    /// deposit was sent (i.e., should be tracked in `processed_tx_hashes`).
    async fn send_if_eligible(
        &self,
        address: &str,
        payment_method_id: &str,
        current_block: u64,
        receive: &UsdcReceive,
        scan_dedup: &HashSet<String>,
    ) -> bool {
        // Ignore dust amounts.
        if receive.amount < USDC_DUST_THRESHOLD {
            info!(
                tx_hash = %receive.tx_hash,
                chain = %self.chain,
                amount = %receive.amount,
                "Skipping dust deposit (< {} USDC)", USDC_DUST_THRESHOLD,
            );
            return false;
        }

        // Deduplication: skip already-processed tx hashes (from persisted
        // state or from earlier receives in this same address scan).
        if self.state.processed_tx_hashes.contains(&receive.tx_hash)
            || scan_dedup.contains(&receive.tx_hash)
        {
            return false;
        }

        // Confirmation depth filter: skip if not enough blocks have passed.
        let required_confirmations = self.config.confirmation_blocks_for(self.chain);
        let confirmations = current_block.saturating_sub(receive.block_number);
        if confirmations < required_confirmations {
            info!(
                tx_hash = %receive.tx_hash,
                chain = %self.chain,
                confirmations,
                required = required_confirmations,
                "Deposit not yet finalized, skipping"
            );
            return false;
        }

        let deposit = EligibleDeposit {
            deposit_address: address.to_string(),
            chain: self.chain,
            tx_hash: receive.tx_hash.clone(),
            usdc_amount: receive.amount,
            payment_method_id: payment_method_id.to_string(),
            deposit_timestamp: receive.fetched_at,
            processed_at: Utc::now(),
        };

        if let Err(e) = self.tx.send(deposit).await {
            error!(
                tx_hash = %receive.tx_hash,
                chain = %self.chain,
                %e,
                "Failed to send deposit to worker — will retry next run"
            );
            return false;
        }

        true
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::coinbase_mock::{MockBlockExplorer, MockCoinbase, MockNotifier};
    use crate::types::{
        BankPaymentOutcome, Chain, LastSeenBlock, SellResult, UsdcReceive, WithdrawalResult,
    };
    use chrono::Duration;
    use rust_decimal_macros::dec;

    /// Build a HashMap with the same mock explorer for all chains.
    fn mock_explorers(explorer: MockBlockExplorer) -> HashMap<Chain, MockBlockExplorer> {
        Chain::iter()
            .map(|c| {
                (c, MockBlockExplorer {
                    receives: explorer.receives.clone(),
                    block_height: explorer.block_height,
                    fail_block_height: explorer.fail_block_height,
                    fail_receives: explorer.fail_receives,
                })
            })
            .collect()
    }

    /// Build a minimal test config.
    fn test_config() -> CryptoSweepConfig {
        use crate::types::DepositAddressConfig;
        CryptoSweepConfig {
            max_auto_amount: dec!(10000.00),
            default_payment_method_id: "pm-default-123".into(),
            deposit_addresses: vec![DepositAddressConfig {
                address: "0xDeposit".into(),
                payment_method_id: "pm-default-123".into(),
            }],
            confirmation_blocks: std::collections::HashMap::new(),
            start_blocks: Chain::iter()
                .map(|c| (c, 0))
                .collect(),
            coinbase_api_key: "key".into(),
            coinbase_api_secret: "secret".into(),
            telegram_bot_token: "bot".into(),
            telegram_chat_id: "chat".into(),
            coinbase_usdc_account_id: None,
            coinbase_usd_account_id: None,
            sell_settle_delay_secs: 0,
            state_file: "/tmp/test-state.json".into(),
        }
    }

    // -- End-to-end deposit check pass tests --

    #[tokio::test]
    async fn deposit_check_processes_eligible_receive() {
        let config = test_config();
        let mut state = PersistedState::default();
        let now = Utc::now();

        let explorer = MockBlockExplorer {
            receives: vec![UsdcReceive {
                tx_hash: "0xabc12345".into(),
                amount: dec!(2400.00),
                block_number: 100,
                fetched_at: now - Duration::hours(1),
            }],
            block_height: 200, // 100 confirmations, well above default 14
            fail_block_height: false,
            fail_receives: false,
        };

        let mut coinbase = MockCoinbase::new();
        coinbase.sell_result = Some(SellResult {
            order_id: "order-1".into(),
            usdc_sold: dec!(2400.00),
            usd_received: dec!(2399.50),
            executed_at: now,
        });
        coinbase.withdrawal_result = Some(WithdrawalResult {
            withdrawal_transfer_id: "wd-1".into(),
            usd_amount: dec!(2399.50),
            initiated_at: now,
        });

        let notifier = MockNotifier::new();

        let explorers = mock_explorers(explorer);
        let pass = DepositCheck::new(&config, &mut state, &explorers, &coinbase, &notifier);
        let errs = pass.run().await;
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");

        assert_eq!(state.bank_payments.len(), 1);
        assert!(state.processed_tx_hashes.contains("0xabc12345"));

        let payment = state.bank_payments.values().next().unwrap();
        assert!(matches!(
            payment.outcome,
            BankPaymentOutcome::Initiated { .. }
        ));
    }

    #[tokio::test]
    async fn deposit_check_skips_too_recent() {
        let config = test_config();
        let mut state = PersistedState::default();
        let now = Utc::now();

        // Deposit has only 5 confirmations, default requires 14.
        let explorer = MockBlockExplorer {
            receives: vec![UsdcReceive {
                tx_hash: "0xrecent".into(),
                amount: dec!(100.00),
                block_number: 100,
                fetched_at: now - Duration::minutes(5),
            }],
            block_height: 105, // only 5 confirmations
            fail_block_height: false,
            fail_receives: false,
        };

        let coinbase = MockCoinbase::new();

        let notifier = MockNotifier::new();

        let explorers = mock_explorers(explorer);
        let pass = DepositCheck::new(&config, &mut state, &explorers, &coinbase, &notifier);
        let errs = pass.run().await;
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");

        // Should not have processed it.
        assert!(state.bank_payments.is_empty());
        assert!(!state.processed_tx_hashes.contains("0xrecent"));
    }

    #[tokio::test]
    async fn deposit_check_threshold_creates_failed() {
        let mut config = test_config();
        config.max_auto_amount = dec!(100.00);
        let mut state = PersistedState::default();
        let now = Utc::now();

        let explorer = MockBlockExplorer {
            receives: vec![UsdcReceive {
                tx_hash: "0xbig".into(),
                amount: dec!(50000.00),
                block_number: 100,
                fetched_at: now - Duration::hours(1),
            }],
            block_height: 200,
            fail_block_height: false,
            fail_receives: false,
        };

        let coinbase = MockCoinbase::new();

        let notifier = MockNotifier::new();

        let explorers = mock_explorers(explorer);
        let pass = DepositCheck::new(&config, &mut state, &explorers, &coinbase, &notifier);
        let errs = pass.run().await;
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");

        assert_eq!(state.bank_payments.len(), 1);
        assert!(state.processed_tx_hashes.contains("0xbig"));

        let payment = state.bank_payments.values().next().unwrap();
        assert!(matches!(
            payment.outcome,
            BankPaymentOutcome::Failed { .. }
        ));

        let msgs = notifier.messages.lock().unwrap();
        assert!(msgs[0].contains("exceeds threshold"));
    }

    #[tokio::test]
    async fn deposit_check_deduplicates() {
        let config = test_config();
        let mut state = PersistedState::default();
        let now = Utc::now();

        // Pre-mark tx as processed.
        state
            .processed_tx_hashes
            .insert("0xalready".to_string());

        let explorer = MockBlockExplorer {
            receives: vec![UsdcReceive {
                tx_hash: "0xalready".into(),
                amount: dec!(100.00),
                block_number: 100,
                fetched_at: now - Duration::hours(1),
            }],
            block_height: 200,
            fail_block_height: false,
            fail_receives: false,
        };

        let coinbase = MockCoinbase::new();

        let notifier = MockNotifier::new();

        let explorers = mock_explorers(explorer);
        let pass = DepositCheck::new(&config, &mut state, &explorers, &coinbase, &notifier);
        let errs = pass.run().await;
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");

        // No new payments should be created.
        assert!(state.bank_payments.is_empty());
    }

    #[tokio::test]
    async fn block_height_failure_suppressed_when_recent() {
        let config = test_config();
        let mut state = PersistedState::default();

        // Recent last_seen_block for all chains — errors should be suppressed.
        let recent = Utc::now() - Duration::hours(1);
        for chain in Chain::iter() {
            state.last_seen_block.insert(chain, LastSeenBlock {
                block_number: 100,
                updated_at: recent,
            });
        }

        let explorer = MockBlockExplorer {
            receives: vec![],
            block_height: 0,
            fail_block_height: true,
            fail_receives: false,
        };

        let coinbase = MockCoinbase::new();
        let notifier = MockNotifier::new();

        let explorers = mock_explorers(explorer);
        let pass = DepositCheck::new(&config, &mut state, &explorers, &coinbase, &notifier);
        let errs = pass.run().await;

        // Errors should be suppressed (not escalated) because data is recent.
        assert!(errs.is_empty(), "expected suppressed errors, got: {errs:?}");
    }

    #[tokio::test]
    async fn block_height_failure_escalated_when_stale() {
        let config = test_config();
        let mut state = PersistedState::default();

        // Stale last_seen_block for all chains — errors should be escalated.
        let stale = Utc::now() - Duration::hours(7);
        for chain in Chain::iter() {
            state.last_seen_block.insert(chain, LastSeenBlock {
                block_number: 100,
                updated_at: stale,
            });
        }

        let explorer = MockBlockExplorer {
            receives: vec![],
            block_height: 0,
            fail_block_height: true,
            fail_receives: false,
        };

        let coinbase = MockCoinbase::new();
        let notifier = MockNotifier::new();

        let explorers = mock_explorers(explorer);
        let pass = DepositCheck::new(&config, &mut state, &explorers, &coinbase, &notifier);
        let errs = pass.run().await;

        // At least the stale chain (Base) should have escalated errors.
        assert!(!errs.is_empty(), "expected escalated errors for stale chain");
        assert!(errs.iter().any(|e| e.contains("block height failed")));
    }

    #[tokio::test]
    async fn block_height_failure_escalated_when_no_previous_data() {
        let config = test_config();
        let mut state = PersistedState::default();
        // No last_seen_block entries — should treat as stale and escalate.

        let explorer = MockBlockExplorer {
            receives: vec![],
            block_height: 0,
            fail_block_height: true,
            fail_receives: false,
        };

        let coinbase = MockCoinbase::new();
        let notifier = MockNotifier::new();

        let explorers = mock_explorers(explorer);
        let pass = DepositCheck::new(&config, &mut state, &explorers, &coinbase, &notifier);
        let errs = pass.run().await;

        assert!(!errs.is_empty(), "expected escalated errors when no previous data");
    }

    #[tokio::test]
    async fn receives_failure_does_not_advance_frontier() {
        let config = test_config();
        let mut state = PersistedState::default();

        // Recent last_seen_block so errors are suppressed, not escalated.
        let recent = Utc::now() - Duration::hours(1);
        for chain in Chain::iter() {
            state.last_seen_block.insert(chain, LastSeenBlock {
                block_number: 100,
                updated_at: recent,
            });
        }

        let explorer = MockBlockExplorer {
            receives: vec![],
            block_height: 200,
            fail_block_height: false,
            fail_receives: true,
        };

        let coinbase = MockCoinbase::new();
        let notifier = MockNotifier::new();

        // Snapshot timestamps before the run.
        let timestamps_before: HashMap<Chain, _> = state
            .last_seen_block
            .iter()
            .map(|(c, lsb)| (*c, lsb.updated_at))
            .collect();

        let explorers = mock_explorers(explorer);
        let pass = DepositCheck::new(&config, &mut state, &explorers, &coinbase, &notifier);
        let errs = pass.run().await;

        // Errors should be suppressed (recent data).
        assert!(errs.is_empty(), "expected suppressed errors, got: {errs:?}");

        // Frontier must NOT have advanced — block numbers and timestamps
        // should be unchanged.
        for chain in Chain::iter() {
            let lsb = state.last_seen_block.get(&chain).expect("last_seen_block missing");
            assert_eq!(
                lsb.block_number, 100,
                "{chain}: frontier advanced despite receives failure"
            );
            assert_eq!(
                lsb.updated_at, timestamps_before[&chain],
                "{chain}: updated_at changed despite receives failure"
            );
        }
    }

    // -- Discovery-only tests (no MockCoinbase needed) --

    #[tokio::test]
    async fn discovery_sends_eligible_deposits_to_channel() {
        let config = test_config();
        let state = PersistedState::default();
        let now = Utc::now();

        let explorer = MockBlockExplorer {
            receives: vec![UsdcReceive {
                tx_hash: "0xchannel1".into(),
                amount: dec!(500.00),
                block_number: 100,
                fetched_at: now - Duration::hours(1),
            }],
            block_height: 200,
            fail_block_height: false,
            fail_receives: false,
        };

        let (tx, mut rx) = mpsc::channel::<EligibleDeposit>(32);

        let check = ChainDepositCheck {
            config: &config,
            state: &state,
            explorer: &explorer,
            chain: Chain::Base,
            tx,
        };

        let result = check.run().await;

        // The tx hash should be tracked in the chain result.
        assert!(result.processed_tx_hashes.contains(&"0xchannel1".to_string()));

        // The deposit should have been sent to the channel.
        let deposit = rx.recv().await.expect("should receive a deposit");
        assert_eq!(deposit.tx_hash, "0xchannel1");
        assert_eq!(deposit.usdc_amount, dec!(500.00));
        assert_eq!(deposit.chain, Chain::Base);

        // Channel should be empty now (sender dropped when check.run() returned).
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn discovery_filters_dust_without_sending() {
        let config = test_config();
        let state = PersistedState::default();
        let now = Utc::now();

        let explorer = MockBlockExplorer {
            receives: vec![UsdcReceive {
                tx_hash: "0xdust".into(),
                amount: dec!(0.50), // below dust threshold
                block_number: 100,
                fetched_at: now - Duration::hours(1),
            }],
            block_height: 200,
            fail_block_height: false,
            fail_receives: false,
        };

        let (tx, mut rx) = mpsc::channel::<EligibleDeposit>(32);

        let check = ChainDepositCheck {
            config: &config,
            state: &state,
            explorer: &explorer,
            chain: Chain::Base,
            tx,
        };

        let result = check.run().await;

        // Nothing should have been sent.
        assert!(result.processed_tx_hashes.is_empty());
        assert!(rx.recv().await.is_none());
    }
}
