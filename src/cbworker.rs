//! Coinbase execution worker.
//!
//! Receives `EligibleDeposit`s from a channel and processes each through
//! the sell + withdraw pipeline. Runs concurrently with chain discovery —
//! the first deposit starts liquidation while other chains are still scanning.

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info};

use crate::types::{
    BankPayment, BankPaymentOutcome, CoinbaseClient, EligibleDeposit, LiquidationResult, Notifier,
};

/// Maximum number of Coinbase sell+withdraw jobs to process concurrently.
const MAX_CONCURRENT_JOBS: usize = 5;

/// Coinbase execution worker. Receives eligible deposits from a channel
/// and processes up to `MAX_CONCURRENT_JOBS` through sell + withdraw
/// concurrently.
pub struct CbWorker<'a, C: CoinbaseClient, N: Notifier> {
    pub coinbase: &'a C,
    pub notifier: &'a N,
    pub max_auto_amount: rust_decimal::Decimal,
    pub sell_settle_delay_secs: u64,
}

impl<C: CoinbaseClient, N: Notifier> CbWorker<'_, C, N> {
    /// Receive eligible deposits from the channel and process through
    /// Coinbase sell + withdraw. Processes up to `MAX_CONCURRENT_JOBS`
    /// deposits concurrently. Returns when the channel closes (all
    /// senders dropped) and all in-flight jobs complete.
    pub async fn run(&self, rx: mpsc::Receiver<EligibleDeposit>) -> Vec<LiquidationResult> {
        ReceiverStream::new(rx)
            .map(|deposit| self.liquidate_one(deposit))
            .buffer_unordered(MAX_CONCURRENT_JOBS)
            .collect()
            .await
    }

    /// Process a single eligible deposit: threshold check, sell, sleep,
    /// withdraw. Returns an `LiquidationResult` with the audit record.
    async fn liquidate_one(&self, deposit: EligibleDeposit) -> LiquidationResult {
        let outcome = self.liquidate_pipeline(&deposit).await;

        let payment = BankPayment {
            id: BankPayment::make_id(&deposit.processed_at, &deposit.tx_hash),
            deposit_address: deposit.deposit_address,
            chain: deposit.chain,
            deposit_tx_hash: deposit.tx_hash.clone(),
            usdc_amount: deposit.usdc_amount,
            payment_method_id: deposit.payment_method_id,
            outcome,
            deposit_timestamp: deposit.deposit_timestamp,
            processed_at: deposit.processed_at,
        };

        LiquidationResult {
            tx_hash: deposit.tx_hash,
            payment,
        }
    }

    /// Run the sell + withdraw pipeline, returning the outcome.
    async fn liquidate_pipeline(&self, deposit: &EligibleDeposit) -> BankPaymentOutcome {
        // Safety check: amount threshold.
        if deposit.usdc_amount > self.max_auto_amount {
            return self.fail(
                format!(
                    "Deposit {} on {} — ${} exceeds threshold ${}. \
                     Recorded as Failed — manual intervention required.",
                    deposit.tx_hash, deposit.chain, deposit.usdc_amount, self.max_auto_amount,
                ),
                deposit.processed_at,
            ).await;
        }

        info!(
            tx_hash = %deposit.tx_hash,
            chain = %deposit.chain,
            amount = %deposit.usdc_amount,
            "Processing deposit"
        );

        let sell_result = match self.coinbase.sell_usdc(deposit.usdc_amount).await {
            Ok(r) => r,
            Err(e) => {
                return self.fail(
                    format!(
                        "Sell failed for deposit {} on {} — ${} USDC. Error: {e}",
                        deposit.tx_hash, deposit.chain, deposit.usdc_amount,
                    ),
                    deposit.processed_at,
                ).await;
            }
        };

        // Wait for the USDC→USD conversion to settle before withdrawal.
        let delay = self.sell_settle_delay_secs;
        info!(delay_secs = delay, "Waiting for sell to settle before withdrawal");
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;

        let withdrawal = match self.coinbase
            .withdraw_usd(sell_result.usd_received, &deposit.payment_method_id)
            .await
        {
            Ok(w) => w,
            Err(e) => {
                return self.fail(
                    format!(
                        "Withdrawal failed for deposit {} on {} — ${} USDC. Error: {e}",
                        deposit.tx_hash, deposit.chain, deposit.usdc_amount,
                    ),
                    deposit.processed_at,
                ).await;
            }
        };

        info!(
            tx_hash = %deposit.tx_hash,
            chain = %deposit.chain,
            amount = %deposit.usdc_amount,
            withdrawal_id = %withdrawal.withdrawal_transfer_id,
            "Deposit sold and withdrawal initiated"
        );

        BankPaymentOutcome::Initiated {
            sell_order_id: sell_result.order_id,
            usd_proceeds: sell_result.usd_received,
            withdrawal_transfer_id: withdrawal.withdrawal_transfer_id,
            initiated_at: withdrawal.initiated_at,
        }
    }

    /// Log an error, send a failure notification, and return a `Failed` outcome.
    async fn fail(
        &self,
        message: String,
        processed_at: chrono::DateTime<chrono::Utc>,
    ) -> BankPaymentOutcome {
        error!(message, "Deposit processing failed");
        if let Err(e) = self.notifier.notify(&message).await {
            error!(%e, "Failed to send failure notification");
        }
        BankPaymentOutcome::Failed { error: message, failed_at: processed_at }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::coinbase_mock::{MockCoinbase, MockNotifier};
    use crate::types::{Chain, SellResult, WithdrawalResult};
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn test_deposit() -> EligibleDeposit {
        let now = Utc::now();
        EligibleDeposit {
            deposit_address: "0xDeposit".into(),
            chain: Chain::Base,
            tx_hash: "0xabc12345".into(),
            usdc_amount: dec!(2400.00),
            payment_method_id: "pm-123".into(),
            deposit_timestamp: now - chrono::Duration::hours(1),
            processed_at: now,
        }
    }

    fn test_worker<'a, C: CoinbaseClient, N: Notifier>(
        coinbase: &'a C,
        notifier: &'a N,
    ) -> CbWorker<'a, C, N> {
        CbWorker {
            coinbase,
            notifier,
            max_auto_amount: dec!(10000.00),
            sell_settle_delay_secs: 0,
        }
    }

    #[tokio::test]
    async fn success_path() {
        let now = Utc::now();
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

        let (tx, rx) = mpsc::channel(32);
        tx.send(test_deposit()).await.unwrap();
        drop(tx);

        let worker = test_worker(&coinbase, &notifier);
        let results = worker.run(rx).await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].payment.outcome,
            BankPaymentOutcome::Initiated { .. }
        ));
        assert_eq!(results[0].tx_hash, "0xabc12345");

        // Verify withdrawal was called with correct payment method.
        let calls = coinbase.withdraw_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "pm-123");
    }

    #[tokio::test]
    async fn sell_failure_creates_failed() {
        let coinbase = MockCoinbase::new(); // no sell_result → error
        let notifier = MockNotifier::new();

        let (tx, rx) = mpsc::channel(32);
        tx.send(test_deposit()).await.unwrap();
        drop(tx);

        let worker = test_worker(&coinbase, &notifier);
        let results = worker.run(rx).await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].payment.outcome,
            BankPaymentOutcome::Failed { .. }
        ));

        let msgs = notifier.messages.lock().unwrap();
        assert!(msgs[0].contains("Sell failed"));
    }

    #[tokio::test]
    async fn withdraw_failure_creates_failed() {
        let now = Utc::now();
        let mut coinbase = MockCoinbase::new();
        coinbase.sell_result = Some(SellResult {
            order_id: "order-1".into(),
            usdc_sold: dec!(2400.00),
            usd_received: dec!(2399.50),
            executed_at: now,
        });
        // no withdrawal_result → error
        let notifier = MockNotifier::new();

        let (tx, rx) = mpsc::channel(32);
        tx.send(test_deposit()).await.unwrap();
        drop(tx);

        let worker = test_worker(&coinbase, &notifier);
        let results = worker.run(rx).await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].payment.outcome,
            BankPaymentOutcome::Failed { .. }
        ));

        let msgs = notifier.messages.lock().unwrap();
        assert!(msgs[0].contains("Withdrawal failed"));
    }

    #[tokio::test]
    async fn threshold_exceeded_creates_failed() {
        let coinbase = MockCoinbase::new();
        let notifier = MockNotifier::new();

        let mut deposit = test_deposit();
        deposit.usdc_amount = dec!(50000.00);

        let (tx, rx) = mpsc::channel(32);
        tx.send(deposit).await.unwrap();
        drop(tx);

        let worker = CbWorker {
            coinbase: &coinbase,
            notifier: &notifier,
            max_auto_amount: dec!(10000.00),
            sell_settle_delay_secs: 0,
        };
        let results = worker.run(rx).await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].payment.outcome,
            BankPaymentOutcome::Failed { .. }
        ));

        let msgs = notifier.messages.lock().unwrap();
        assert!(msgs[0].contains("exceeds threshold"));
    }

    #[tokio::test]
    async fn empty_channel_returns_empty() {
        let coinbase = MockCoinbase::new();
        let notifier = MockNotifier::new();

        let (_tx, rx) = mpsc::channel::<EligibleDeposit>(32);
        drop(_tx);

        let worker = test_worker(&coinbase, &notifier);
        let results = worker.run(rx).await;

        assert!(results.is_empty());
    }
}
