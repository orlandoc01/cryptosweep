//! Coinbase client: exchange-side operations for the USDC deposit check flow.
//!
//! Implements the `CoinbaseClient` trait from `types.rs` using the Coinbase
//! v2 and v3 (brokerage) APIs with JWT ES256 authentication.
//!
//! ## Operations
//! 1. **Sell USDC** — convert USDC → USD via `/api/v3/brokerage/convert`
//! 2. **Withdrawal** — withdraw USD to linked payment method via `/v2/accounts/{id}/withdrawals`

pub mod auth;
pub mod models;

use reqwest::Client;
use rust_decimal::Decimal;
use std::str::FromStr;
use strum::Display;
use tracing::{error, info};

/// HTTP method for Coinbase API requests.
/// Used by `send()` and `build_jwt()` to avoid stringly-typed dispatch.
#[derive(Clone, Copy, Display)]
#[strum(serialize_all = "UPPERCASE")]
pub(crate) enum HttpMethod {
    Get,
    Post,
}

use crate::types::{CoinbaseClient, AppError, SellResult, WithdrawalResult};
use auth::{build_jwt, parse_pem_key};
use models::{
    CoinbaseAccount, ConvertCommitRequest, ConvertQuoteRequest, ConvertQuoteResponse,
    ConvertTradeResponse, TransferResponse, V2DataResponse, V2ListResponse, WithdrawalRequest,
};

/// Base URL for the Coinbase API.
const COINBASE_API_BASE: &str = "https://api.coinbase.com";

/// Live implementation of `CoinbaseClient` that talks to the real Coinbase API.
///
/// Holds a `reqwest::Client` and the API credentials. Constructed once per
/// cron invocation.
pub struct LiveCoinbaseClient {
    client: Client,
    /// CDP key name (e.g., `"organizations/{org_id}/apiKeys/{key_id}"`).
    api_key: String,
    /// ECDSA P-256 signing key, parsed once from PEM at construction time.
    signing_key: p256::ecdsa::SigningKey,
    /// Pre-configured USDC account UUID (skips account list pagination).
    usdc_account_id: Option<String>,
    /// Pre-configured USD account UUID (skips account list pagination).
    usd_account_id: Option<String>,
}

impl LiveCoinbaseClient {
    /// Create a new Coinbase client with the given CDP credentials.
    ///
    /// Parses the PEM-encoded ECDSA P-256 private key once upfront.
    /// Returns `AppError::Coinbase` if the key is invalid.
    pub fn new(
        api_key: String,
        api_secret: &str,
        usdc_account_id: Option<String>,
        usd_account_id: Option<String>,
    ) -> Result<Self, AppError> {
        let signing_key = parse_pem_key(api_secret)?;
        Ok(Self {
            client: Client::new(),
            api_key,
            signing_key,
            usdc_account_id,
            usd_account_id,
        })
    }

    /// Build an authenticated request and return the raw response body.
    ///
    /// Handles JWT signing, URL construction, status checking, and body
    /// extraction — the shared core of all Coinbase API calls.
    async fn send(
        &self,
        method: HttpMethod,
        path: &str,
        json_body: Option<serde_json::Value>,
    ) -> Result<String, AppError> {
        // JWT URI must use only the path portion, not query parameters.
        let jwt_path = path.split('?').next().unwrap_or(path);
        let jwt = build_jwt(&self.api_key, &self.signing_key, method, jwt_path)?;
        let url = format!("{COINBASE_API_BASE}{path}");

        let mut req = match method {
            HttpMethod::Get => self.client.get(&url),
            HttpMethod::Post => self.client.post(&url),
        };
        req = req.header("Authorization", format!("Bearer {jwt}"));
        if let Some(body) = json_body {
            req = req.json(&body);
        }

        let resp = req.send().await?;
        let status = resp.status();
        let raw_body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(AppError::Coinbase(format!(
                "{method} {path} failed — HTTP {status}: {raw_body}"
            )));
        }

        Ok(raw_body)
    }

    /// Deserialize a raw JSON response body, logging the raw text on failure.
    fn deserialize<T: serde::de::DeserializeOwned>(
        method: HttpMethod,
        path: &str,
        raw_body: &str,
    ) -> Result<T, AppError> {
        serde_json::from_str::<T>(raw_body).map_err(|e| {
            error!(
                %e,
                path,
                body = %raw_body,
                "Failed to deserialize Coinbase response"
            );
            AppError::Coinbase(format!(
                "{method} {path} — deserialization failed: {e}"
            ))
        })
    }

    /// Send an authenticated GET request to the Coinbase API.
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, AppError> {
        let raw_body = self.send(HttpMethod::Get, path, None).await?;
        Self::deserialize(HttpMethod::Get, path, &raw_body)
    }

    /// Send an authenticated GET request and return raw response bytes.
    ///
    /// Used by cassette-based tests to record the raw JSON before
    /// deserialization.
    #[cfg(test)]
    async fn get_raw(&self, path: &str) -> Result<Vec<u8>, AppError> {
        self.send(HttpMethod::Get, path, None).await.map(|s| s.into_bytes())
    }

    /// Send an authenticated POST request and return raw response bytes.
    ///
    /// Used by cassette-based tests to record the raw JSON before
    /// deserialization.
    #[cfg(test)]
    async fn post_raw<B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<Vec<u8>, AppError> {
        let json_body = serde_json::to_value(body)
            .map_err(|e| AppError::Coinbase(format!("Failed to serialize request: {e}")))?;
        self.send(HttpMethod::Post, path, Some(json_body)).await.map(|s| s.into_bytes())
    }

    /// Send an authenticated POST request with a JSON body.
    async fn post<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, AppError> {
        let json_body = serde_json::to_value(body)
            .map_err(|e| AppError::Coinbase(format!("Failed to serialize request: {e}")))?;
        let raw_body = self.send(HttpMethod::Post, path, Some(json_body)).await?;
        Self::deserialize(HttpMethod::Post, path, &raw_body)
    }

    /// Find a Coinbase account by currency code (e.g., "USDC" or "USD").
    ///
    /// If a pre-configured account UUID is set for this currency, fetches it
    /// directly via `/v2/accounts/{id}` (one API call, no pagination).
    /// Otherwise falls back to listing all accounts with `limit=300`.
    async fn find_account(&self, currency: &str) -> Result<CoinbaseAccount, AppError> {
        // Use pre-configured account ID if available.
        let configured_id = match currency {
            "USDC" => self.usdc_account_id.as_deref(),
            "USD" => self.usd_account_id.as_deref(),
            _ => None,
        };

        if let Some(id) = configured_id {
            let path = format!("/v2/accounts/{id}");
            let resp: V2DataResponse<CoinbaseAccount> = self.get(&path).await?;
            return Ok(resp.data);
        }

        // Fallback: list all accounts.
        let resp: V2ListResponse<CoinbaseAccount> =
            self.get("/v2/accounts?limit=300").await?;

        resp.data
            .into_iter()
            .find(|a| a.currency.code == currency)
            .ok_or_else(|| {
                AppError::Coinbase(format!("No {currency} account found on Coinbase"))
            })
    }
}

impl CoinbaseClient for LiveCoinbaseClient {
    /// Convert USDC to USD via the v3 brokerage convert API.
    ///
    /// USDC↔USD is a 1:1 stablecoin conversion, not a market trade. The v3
    /// brokerage has no `USDC-USD` trading product. Instead, Coinbase handles
    /// this through the convert endpoints (quote → commit).
    async fn sell_usdc(&self, amount: Decimal) -> Result<SellResult, AppError> {
        // Step 1: Get a conversion quote.
        let quote_request = ConvertQuoteRequest {
            from_account: "USDC".to_string(),
            to_account: "USD".to_string(),
            amount: amount.to_string(),
        };

        info!(amount = %amount, "Requesting USDC → USD conversion quote");

        let quote_resp: ConvertQuoteResponse = self
            .post("/api/v3/brokerage/convert/quote", &quote_request)
            .await?;

        let trade = quote_resp.trade.ok_or_else(|| {
            AppError::Coinbase("Convert quote returned no trade object".into())
        })?;

        let trade_id = trade.id.ok_or_else(|| {
            AppError::Coinbase("Convert quote returned no trade ID".into())
        })?;

        let fee_str = trade
            .total_fee
            .as_ref()
            .and_then(|f| f.amount.as_ref())
            .and_then(|a| a.value.as_deref())
            .unwrap_or("0");

        info!(
            trade_id = %trade_id,
            status = ?trade.status,
            fee = %fee_str,
            "Conversion quote received, committing trade"
        );

        // Step 2: Commit the trade.
        let commit_request = ConvertCommitRequest {
            from_account: "USDC".to_string(),
            to_account: "USD".to_string(),
        };

        let commit_path = format!("/api/v3/brokerage/convert/trade/{trade_id}");
        let commit_resp: ConvertTradeResponse =
            self.post(&commit_path, &commit_request).await?;

        let committed_trade = commit_resp.trade.ok_or_else(|| {
            AppError::Coinbase("Convert commit returned no trade object".into())
        })?;

        let status = committed_trade
            .status
            .as_deref()
            .unwrap_or("unknown");

        let usd_received = committed_trade
            .total
            .as_ref()
            .and_then(|a| a.value.as_deref())
            .and_then(|v| Decimal::from_str(v).ok())
            .ok_or_else(|| AppError::Coinbase(
                "Convert commit response missing USD total".into(),
            ))?;

        let usdc_sold = committed_trade
            .user_entered_amount
            .as_ref()
            .and_then(|a| a.value.as_deref())
            .and_then(|v| Decimal::from_str(v).ok())
            .ok_or_else(|| AppError::Coinbase(
                "Convert commit response missing USDC sold amount".into(),
            ))?;

        info!(
            trade_id = %trade_id,
            usdc_sold = %usdc_sold,
            usd_received = %usd_received,
            status = %status,
            "USDC → USD conversion committed"
        );

        Ok(SellResult {
            order_id: trade_id,
            usdc_sold,
            usd_received,
            executed_at: chrono::Utc::now(),
        })
    }

    /// Initiate a withdrawal from the USD account to the specified payment method.
    ///
    /// Uses `commit: true` to execute in one step (no separate commit call).
    async fn withdraw_usd(
        &self,
        amount: Decimal,
        payment_method_id: &str,
    ) -> Result<WithdrawalResult, AppError> {
        // Use the pre-configured USD account ID directly if available,
        // avoiding an extra API call per withdrawal.
        let usd_account_id = match &self.usd_account_id {
            Some(id) => id.clone(),
            None => self.find_account("USD").await?.id,
        };

        let path = format!("/v2/accounts/{usd_account_id}/withdrawals");

        let request = WithdrawalRequest {
            amount: amount.to_string(),
            currency: "USD".to_string(),
            payment_method: payment_method_id.to_string(),
            commit: true,
        };

        info!(
            amount = %amount,
            usd_account = %usd_account_id,
            payment_method = %payment_method_id,
            "Initiating withdrawal"
        );

        let resp: TransferResponse = self.post(&path, &request).await?;
        let transfer = resp.transfer;

        // Detect genuine failures: the API returns 200 with a
        // cancellation_reason and an empty transfer ID when the withdrawal
        // is actually rejected (e.g., insufficient funds).
        if let Some(reason) = &transfer.cancellation_reason
            && transfer.id.is_empty()
        {
            return Err(AppError::Coinbase(format!(
                "Withdrawal rejected: {}", reason.message
            )));
        }

        if transfer.id.is_empty() {
            return Err(AppError::Coinbase(
                "Withdrawal returned no transfer ID".into(),
            ));
        }

        let amount_str = transfer
            .amount
            .as_ref()
            .and_then(|a| a.value.as_deref())
            .unwrap_or("unknown");

        info!(
            withdrawal_id = %transfer.id,
            amount = %amount_str,
            "Withdrawal initiated"
        );

        Ok(WithdrawalResult {
            withdrawal_transfer_id: transfer.id,
            usd_amount: amount,
            initiated_at: chrono::Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::models::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    /// Integration test: verifies Coinbase API auth and account listing.
    #[tokio::test]
    async fn coinbase_api_auth_smoke() {
        use super::*;

        use crate::tests::cassette;

        let config = crate::config::load_test_config();

        let client = LiveCoinbaseClient::new(
            config.coinbase_api_key,
            &config.coinbase_api_secret,
            config.coinbase_usdc_account_id,
            config.coinbase_usd_account_id,
        )
        .expect("valid Coinbase credentials");

        let resp: V2ListResponse<CoinbaseAccount> = cassette::replay_or_record(
            "src/tests/fixtures/coinbase_api_auth.cassette.json",
            || async {
                client
                    .get_raw("/v2/accounts?limit=300")
                    .await
                    .map_err(|e| format!("API call failed: {e}"))
            },
        )
        .await
        .expect("Coinbase API auth should succeed (live or cassette)");

        assert!(!resp.data.is_empty(), "should have at least one account");

        let has_usdc = resp.data.iter().any(|a| a.currency.code == "USDC");
        let has_usd = resp.data.iter().any(|a| a.currency.code == "USD");
        assert!(has_usdc, "should have a USDC account");
        assert!(has_usd, "should have a USD account");
    }

    /// Utility test: discovers Coinbase account UUIDs for config.
    /// Run with: `cargo test coinbase_find_account_ids -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn coinbase_find_account_ids() {
        use super::*;

        let config = crate::config::load_test_config();

        let client = LiveCoinbaseClient::new(
            config.coinbase_api_key,
            &config.coinbase_api_secret,
            config.coinbase_usdc_account_id,
            config.coinbase_usd_account_id,
        )
        .expect("valid Coinbase credentials");

        let usdc = client.find_account("USDC").await
            .expect("USDC account should exist");
        let usd = client.find_account("USD").await
            .expect("USD account should exist");

        println!("coinbase_usdc_account_id = \"{}\"\ncoinbase_usd_account_id = \"{}\"", usdc.id, usd.id);
    }

    /// Utility test: lists withdrawable payment methods on the Coinbase account.
    /// Run with: `cargo test coinbase_list_withdrawable_payment_methods -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn coinbase_list_withdrawable_payment_methods() {
        use super::*;

        let config = crate::config::load_test_config();

        let client = LiveCoinbaseClient::new(
            config.coinbase_api_key,
            &config.coinbase_api_secret,
            config.coinbase_usdc_account_id,
            config.coinbase_usd_account_id,
        )
        .expect("valid Coinbase credentials");

        let resp: models::V3PaymentMethodsResponse = serde_json::from_slice(
            &client
                .get_raw("/api/v3/brokerage/payment_methods")
                .await
                .expect("GET /api/v3/brokerage/payment_methods should succeed"),
        )
        .expect("response should deserialize");

        let withdrawable: Vec<_> = resp.payment_methods
            .iter()
            .filter(|pm| pm.allow_withdraw == Some(true))
            .collect();

        for pm in &withdrawable {
            println!(
                "id={:<40} type={:<20} name={}",
                pm.id,
                pm.method_type,
                pm.name,
            );
        }

        assert!(!withdrawable.is_empty(), "should have at least one withdrawable payment method");
    }

    /// Smoke test: converts USDC → USD via the v3 convert API.
    #[tokio::test]
    async fn coinbase_convert_usdc_smoke() {
        use super::*;

        use crate::tests::cassette::Cassette;
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let config = crate::config::load_test_config();

        let client = LiveCoinbaseClient::new(
            config.coinbase_api_key,
            &config.coinbase_api_secret,
            config.coinbase_usdc_account_id,
            config.coinbase_usd_account_id,
        )
        .expect("valid Coinbase credentials");

        let mut cassette =
            Cassette::load_or_new("src/tests/fixtures/coinbase_convert_usdc.cassette.json");

        // Step 1: Quote
        let quote_request = models::ConvertQuoteRequest {
            from_account: "USDC".to_string(),
            to_account: "USD".to_string(),
            amount: "5.00".to_string(),
        };

        let quote_resp: models::ConvertQuoteResponse = cassette
            .next_or_record(|| async {
                client
                    .post_raw("/api/v3/brokerage/convert/quote", &quote_request)
                    .await
                    .map_err(|e| format!("{e}"))
            })
            .await
            .expect("Convert quote should succeed");

        let trade = quote_resp.trade.expect("Quote should return a trade");
        let trade_id = trade.id.expect("Trade should have an ID");
        assert!(!trade_id.is_empty());

        // Step 2: Commit
        let commit_request = models::ConvertCommitRequest {
            from_account: "USDC".to_string(),
            to_account: "USD".to_string(),
        };

        let commit_path = format!("/api/v3/brokerage/convert/trade/{trade_id}");
        let commit_resp: models::ConvertTradeResponse = cassette
            .next_or_record(|| async {
                client
                    .post_raw(&commit_path, &commit_request)
                    .await
                    .map_err(|e| format!("{e}"))
            })
            .await
            .expect("Convert commit should succeed");

        cassette.save().expect("Cassette save should succeed");

        let committed = commit_resp.trade.expect("Commit should return a trade");
        let usd_received = committed
            .total
            .as_ref()
            .and_then(|a| a.value.as_deref())
            .and_then(|v| Decimal::from_str(v).ok())
            .expect("Should have USD total");

        assert!(usd_received > Decimal::ZERO, "should have received some USD");
    }

    /// Smoke test: initiates a withdrawal from the USD account.
    #[tokio::test]
    async fn coinbase_withdraw_smoke() {
        use super::*;

        use crate::tests::cassette::Cassette;

        let config = crate::config::load_test_config();

        let payment_method_id = config.default_payment_method_id.clone();

        let client = LiveCoinbaseClient::new(
            config.coinbase_api_key,
            &config.coinbase_api_secret,
            config.coinbase_usdc_account_id.clone(),
            config.coinbase_usd_account_id.clone(),
        )
        .expect("valid Coinbase credentials");

        let mut cassette =
            Cassette::load_or_new("src/tests/fixtures/coinbase_withdraw.cassette.json");

        // Step 1: Resolve USD account.
        let usd_account_id = config
            .coinbase_usd_account_id
            .as_deref()
            .unwrap_or("ffffffff-1111-2222-3333-444444444444");

        let account_path = format!("/v2/accounts/{usd_account_id}");
        let account_resp: V2DataResponse<CoinbaseAccount> = cassette
            .next_or_record(|| async {
                client
                    .get_raw(&account_path)
                    .await
                    .map_err(|e| format!("{e}"))
            })
            .await
            .expect("USD account lookup should succeed");
        assert_eq!(account_resp.data.currency.code, "USD");

        // Step 2: Initiate withdrawal.
        let withdraw_path = format!("/v2/accounts/{}/withdrawals", account_resp.data.id);
        let withdraw_body = WithdrawalRequest {
            amount: "5.00".to_string(),
            currency: "USD".to_string(),
            payment_method: payment_method_id,
            commit: true,
        };

        let transfer_resp: TransferResponse = cassette
            .next_or_record(|| async {
                client
                    .post_raw(&withdraw_path, &withdraw_body)
                    .await
                    .map_err(|e| format!("{e}"))
            })
            .await
            .expect("Withdrawal should succeed");

        cassette.save().expect("Cassette save should succeed");

        assert!(
            !transfer_resp.transfer.id.is_empty(),
            "should have a transfer ID"
        );
        assert!(
            transfer_resp.transfer.cancellation_reason.is_none(),
            "should not have a cancellation reason"
        );
    }

    #[test]
    fn find_usdc_account_from_list() {
        let accounts = vec![
            CoinbaseAccount {
                id: "acc-btc".into(),
                name: "BTC Wallet".into(),
                currency: CurrencyInfo { code: "BTC".into() },
                balance: MoneyAmount {
                    amount: "0.5".into(),
                    currency: "BTC".into(),
                },
            },
            CoinbaseAccount {
                id: "acc-usdc".into(),
                name: "USDC Wallet".into(),
                currency: CurrencyInfo {
                    code: "USDC".into(),
                },
                balance: MoneyAmount {
                    amount: "3847.21".into(),
                    currency: "USDC".into(),
                },
            },
        ];

        let usdc = accounts.iter().find(|a| a.currency.code == "USDC");
        assert!(usdc.is_some());
        let balance = Decimal::from_str(&usdc.unwrap().balance.amount).unwrap();
        assert_eq!(balance, Decimal::from_str("3847.21").unwrap());
    }

}
