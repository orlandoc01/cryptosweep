//! Shared mock implementations of `CoinbaseClient`, `BlockExplorer`,
//! and `Notifier` for integration tests.


use std::sync::Mutex;

use rust_decimal::Decimal;

use crate::types::{
    BlockExplorer, CoinbaseClient, Notifier,
    AppError, SellResult, UsdcReceive, WithdrawalResult,
};

/// Mock `BlockExplorer` with configurable responses.
///
/// Set `fail_block_height` or `fail_receives` to `true` to simulate
/// transient RPC failures for staleness-based alerting tests.
pub struct MockBlockExplorer {
    pub receives: Vec<UsdcReceive>,
    pub block_height: u64,
    pub fail_block_height: bool,
    pub fail_receives: bool,
}

impl BlockExplorer for MockBlockExplorer {
    async fn get_block_height(&self) -> Result<u64, AppError> {
        if self.fail_block_height {
            return Err(AppError::Explorer("mock: block height failure".into()));
        }
        Ok(self.block_height)
    }

    async fn fetch_usdc_receives(
        &self,
        _address: &str,
        _since_block: u64,
    ) -> Result<Vec<UsdcReceive>, AppError> {
        if self.fail_receives {
            return Err(AppError::Explorer("mock: fetch receives failure".into()));
        }
        Ok(self.receives.clone())
    }
}

/// Mock `CoinbaseClient` with configurable responses and call recording.
///
/// - `sell_result`, `withdrawal_result`: set to return values.
/// - `withdraw_calls`: records every `(amount, payment_method_id)` pair
///   passed to `withdraw_usd`, so tests can assert withdrawal routing.
pub struct MockCoinbase {
    pub sell_result: Option<SellResult>,
    pub withdrawal_result: Option<WithdrawalResult>,
    /// Records `(amount, payment_method_id)` for each `withdraw_usd` call.
    pub withdraw_calls: Mutex<Vec<(Decimal, String)>>,
}

impl MockCoinbase {
    /// Create a new mock with no responses configured and empty call log.
    pub fn new() -> Self {
        Self {
            sell_result: None,
            withdrawal_result: None,
            withdraw_calls: Mutex::new(Vec::new()),
        }
    }
}

impl CoinbaseClient for MockCoinbase {
    async fn sell_usdc(&self, _amount: Decimal) -> Result<SellResult, AppError> {
        self.sell_result
            .clone()
            .ok_or_else(|| AppError::Coinbase("Mock: no sell result".into()))
    }

    async fn withdraw_usd(
        &self,
        amount: Decimal,
        payment_method_id: &str,
    ) -> Result<WithdrawalResult, AppError> {
        // Record the call for assertion.
        self.withdraw_calls
            .lock()
            .unwrap()
            .push((amount, payment_method_id.to_string()));

        self.withdrawal_result
            .clone()
            .ok_or_else(|| AppError::Coinbase("Mock: no withdrawal result".into()))
    }
}

/// Mock `Notifier` that records all messages for assertion.
pub struct MockNotifier {
    pub messages: Mutex<Vec<String>>,
}

impl MockNotifier {
    pub fn new() -> Self {
        Self {
            messages: Mutex::new(Vec::new()),
        }
    }
}

impl Notifier for MockNotifier {
    async fn notify(&self, message: &str) -> Result<(), AppError> {
        self.messages.lock().unwrap().push(message.to_string());
        Ok(())
    }
}
