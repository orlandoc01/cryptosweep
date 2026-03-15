//! Coinbase API request and response types.
//!
//! These structs model the subset of the Coinbase v2 and v3 APIs that
//! cryptosweep uses. Response structs include all fields returned by the API
//! for deserialization completeness — some fields are only read in tests or
//! logs (via `Debug`), not in production code paths.
//!
//! ## Response conventions
//! - v2 API: single resources in `{"data": {...}}`, lists in `{"data": [...]}`
//! - Monetary amounts are decimal strings: `{"amount": "1236.63", "currency": "USD"}`

// Response structs deserialize all API fields for completeness; not all are
// read in production code paths.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// v2 API — Accounts
// ---------------------------------------------------------------------------

/// Wrapper for v2 list responses: `{"data": [...]}`.
#[derive(Debug, Deserialize)]
pub struct V2ListResponse<T> {
    pub data: Vec<T>,
}

/// Wrapper for v2 single-resource responses: `{"data": {...}}`.
#[derive(Debug, Deserialize)]
pub struct V2DataResponse<T> {
    pub data: T,
}

/// A Coinbase account from `/v2/accounts`.
/// We use this to find the USDC account (for deposit/balance check)
/// and the USD account (for withdrawal).
#[derive(Debug, Deserialize)]
pub struct CoinbaseAccount {
    pub id: String,
    pub name: String,
    pub currency: CurrencyInfo,
    pub balance: MoneyAmount,
}

/// Currency metadata nested in a Coinbase account.
#[derive(Debug, Deserialize)]
pub struct CurrencyInfo {
    pub code: String,
}

/// Monetary amount as a decimal string + currency code.
/// Used throughout the v2 API for balances, amounts, etc.
#[derive(Debug, Deserialize)]
pub struct MoneyAmount {
    pub amount: String,
    pub currency: String,
}

// ---------------------------------------------------------------------------
// v2 API — Withdrawals
// ---------------------------------------------------------------------------

/// Request body for `POST /v2/accounts/{account_id}/withdrawals`.
#[derive(Debug, Serialize)]
pub struct WithdrawalRequest {
    pub amount: String,
    pub currency: String,
    pub payment_method: String,
    /// Commit in one step (no separate commit call needed).
    pub commit: bool,
}

/// Wrapper for withdrawal POST responses.
/// The v2 withdrawal endpoint wraps in `"transfer"`, not `"data"` like
/// other v2 endpoints.
#[derive(Debug, Deserialize)]
pub struct TransferResponse {
    pub transfer: WithdrawalTransfer,
}

/// The transfer object inside a withdrawal response.
///
/// The v2 withdrawal endpoint returns monetary amounts using
/// `{"value": "...", "currency": "..."}` (like the v3 convert API),
/// NOT the `{"amount": "...", "currency": "..."}` format used by other
/// v2 endpoints. We only deserialize the fields we actually need and
/// ignore the rest — the API returns many extra fields that vary between
/// endpoint versions.
#[derive(Debug, Deserialize)]
pub struct WithdrawalTransfer {
    pub id: String,
    /// Withdrawal amount. Uses `value`/`currency` format, not `amount`/`currency`.
    pub amount: Option<ConvertAmount>,
    committed: Option<bool>,
    /// Status of the withdrawal: "pending", "completed", "failed", etc.
    #[serde(default)]
    status: String,
    /// Present when the withdrawal was rejected (e.g., insufficient funds).
    pub cancellation_reason: Option<CancellationReason>,
}

/// Reason a withdrawal was rejected by Coinbase.
#[derive(Debug, Deserialize)]
pub struct CancellationReason {
    pub message: String,
    #[serde(default)]
    pub code: String,
}

// ---------------------------------------------------------------------------
// v3 Brokerage API — Payment Methods
// ---------------------------------------------------------------------------

/// Response from `GET /api/v3/brokerage/payment_methods`.
#[derive(Debug, Deserialize)]
pub struct V3PaymentMethodsResponse {
    pub payment_methods: Vec<PaymentMethod>,
}

/// A payment method from the v3 brokerage API.
#[derive(Debug, Deserialize)]
pub struct PaymentMethod {
    pub id: String,
    pub name: String,
    /// e.g., "ACH_BANK_ACCOUNT", "FEDWIRE", "SECURE3D_CARD"
    #[serde(rename = "type")]
    pub method_type: String,
    pub currency: Option<String>,
    pub allow_withdraw: Option<bool>,
    pub allow_deposit: Option<bool>,
}

// ---------------------------------------------------------------------------
// v3 Brokerage API — Convert (USDC ↔ USD)
// ---------------------------------------------------------------------------

/// Request body for `POST /api/v3/brokerage/convert/quote`.
///
/// Note: `from_account` and `to_account` are **currency codes** (e.g.,
/// `"USDC"`, `"USD"`), NOT account UUIDs. The API resolves them internally.
#[derive(Debug, Serialize)]
pub struct ConvertQuoteRequest {
    pub from_account: String,
    pub to_account: String,
    /// Amount to convert, as a decimal string in the source currency.
    pub amount: String,
}

/// Response from `POST /api/v3/brokerage/convert/quote`.
#[derive(Debug, Deserialize)]
pub struct ConvertQuoteResponse {
    pub trade: Option<ConvertTrade>,
}

/// Request body for `POST /api/v3/brokerage/convert/trade/{trade_id}`.
/// Commits a previously quoted conversion.
#[derive(Debug, Serialize)]
pub struct ConvertCommitRequest {
    pub from_account: String,
    pub to_account: String,
}

/// Response from committing or fetching a conversion trade.
#[derive(Debug, Deserialize)]
pub struct ConvertTradeResponse {
    pub trade: Option<ConvertTrade>,
}

/// A conversion trade record from the v3 convert API.
#[derive(Debug, Deserialize)]
pub struct ConvertTrade {
    pub id: Option<String>,
    /// e.g., "TRADE_STATUS_CREATED", "TRADE_STATUS_COMPLETED".
    pub status: Option<String>,
    /// The amount the user entered (source currency).
    pub user_entered_amount: Option<ConvertAmount>,
    /// The total amount in target currency after fees.
    pub total: Option<ConvertAmount>,
    /// The subtotal before fees.
    pub subtotal: Option<ConvertAmount>,
    /// Fees charged for the conversion.
    pub total_fee: Option<ConvertFee>,
    pub source_currency: Option<String>,
    pub target_currency: Option<String>,
    pub exchange_rate: Option<ConvertAmount>,
}

/// Monetary amount in convert API responses.
#[derive(Debug, Deserialize)]
pub struct ConvertAmount {
    pub value: Option<String>,
    pub currency: Option<String>,
}

/// Fee information in convert API responses.
#[derive(Debug, Deserialize)]
pub struct ConvertFee {
    pub title: Option<String>,
    pub amount: Option<ConvertAmount>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_v2_accounts_list() {
        let json = r#"{
            "data": [
                {
                    "id": "acc-usdc-123",
                    "name": "USDC Wallet",
                    "currency": { "code": "USDC" },
                    "balance": { "amount": "250.00", "currency": "USDC" }
                },
                {
                    "id": "acc-usd-456",
                    "name": "USD Wallet",
                    "currency": { "code": "USD" },
                    "balance": { "amount": "50.00", "currency": "USD" }
                }
            ]
        }"#;

        let resp: V2ListResponse<CoinbaseAccount> =
            serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].currency.code, "USDC");
        assert_eq!(resp.data[0].balance.amount, "250.00");
        assert_eq!(resp.data[1].currency.code, "USD");
    }

    #[test]
    fn deserialize_withdrawal_transfer_response() {
        let json = r#"{
            "transfer": {
                "id": "wd-abc-123",
                "amount": { "value": "250.00", "currency": "USD" },
                "committed": true,
                "status": "pending"
            }
        }"#;

        let resp: TransferResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.transfer.id, "wd-abc-123");
        assert_eq!(
            resp.transfer.amount.as_ref().unwrap().value.as_deref(),
            Some("250.00")
        );
        assert_eq!(resp.transfer.committed, Some(true));
        assert!(resp.transfer.cancellation_reason.is_none());
    }

    /// Verify that the real Coinbase withdrawal response (with many extra
    /// fields) deserializes without error. Serde ignores unknown fields by
    /// default — this test ensures we don't accidentally break that.
    #[test]
    fn deserialize_real_withdrawal_response() {
        let json = r#"{"transfer":{"user_entered_amount":{"value":"123.45","currency":"USD"},"amount":{"value":"123.45","currency":"USD"},"total":{"value":"123.45","currency":"USD"},"subtotal":{"value":"123.45","currency":"USD"},"idem":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","committed":false,"id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","instant":false,"source":{"type":"LEDGER_ACCOUNT","network":"internal_retail"},"target":{"type":"EXTERNAL_PAYMENT_METHOD","network":"ach"},"payout_at":"2099-01-01T00:00:00.000000000Z","status":"","user_reference":"XXXXXXXXXX","type":"TRANSFER_TYPE_WITHDRAWAL","created_at":null,"updated_at":null,"user_warnings":[],"fees":[],"total_fee":{"title":"Fee Total","description":"Total fee associated with this transaction","amount":{"value":"0.00","currency":"USD"},"type":"COINBASE"},"cancellation_reason":null,"hold_days":0,"nextStep":null,"checkout_url":"","requires_completion_step":false,"transfer_settings":{"delay_until":"0001-01-01T00:00:00Z","time_delay_in_hours":0,"challenge_required":false}}}"#;

        let resp: TransferResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.transfer.id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(
            resp.transfer.amount.as_ref().unwrap().value.as_deref(),
            Some("123.45")
        );
        assert!(!resp.transfer.committed.unwrap());
    }

    #[test]
    fn deserialize_withdrawal_soft_failure() {
        let json = r#"{
            "transfer": {
                "id": "",
                "amount": null,
                "committed": false,
                "status": "",
                "cancellation_reason": {
                    "message": "Amount is too large. You don't have enough funds in your account.",
                    "code": "ERROR_CODES_INVALID"
                },
                "fees": [],
                "user_warnings": []
            }
        }"#;

        let resp: TransferResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.transfer.committed, Some(false));
        assert!(resp.transfer.cancellation_reason.is_some());
        assert!(resp.transfer.cancellation_reason.unwrap().message.contains("too large"));
    }

    #[test]
    fn serialize_withdrawal_request() {
        let req = WithdrawalRequest {
            amount: "250.00".into(),
            currency: "USD".into(),
            payment_method: "pm-ach-789".into(),
            commit: true,
        };

        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["amount"], "250.00");
        assert_eq!(json["currency"], "USD");
        assert_eq!(json["commit"], true);
    }

    #[test]
    fn serialize_convert_quote_request() {
        let req = ConvertQuoteRequest {
            from_account: "USDC".into(),
            to_account: "USD".into(),
            amount: "36.88".into(),
        };

        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["from_account"], "USDC");
        assert_eq!(json["to_account"], "USD");
        assert_eq!(json["amount"], "36.88");
    }

    #[test]
    fn deserialize_convert_quote_response() {
        let json = r#"{
            "trade": {
                "id": "trade-123",
                "status": "TRADE_STATUS_UNSPECIFIED",
                "user_entered_amount": { "value": "36.88", "currency": "USDC" },
                "total": { "value": "36.88", "currency": "USD" },
                "subtotal": { "value": "36.88", "currency": "USD" },
                "total_fee": {
                    "title": "Coinbase Fee",
                    "amount": { "value": "0", "currency": "USD" }
                },
                "source_currency": "USDC",
                "target_currency": "USD",
                "exchange_rate": { "value": "1", "currency": "USDC" }
            }
        }"#;

        let resp: ConvertQuoteResponse = serde_json::from_str(json).expect("deserialize");
        let trade = resp.trade.unwrap();
        assert_eq!(trade.id, Some("trade-123".into()));
        assert_eq!(trade.status, Some("TRADE_STATUS_UNSPECIFIED".into()));
        assert_eq!(
            trade.user_entered_amount.unwrap().value,
            Some("36.88".into())
        );
        assert_eq!(trade.total.unwrap().value, Some("36.88".into()));
        assert_eq!(trade.source_currency, Some("USDC".into()));
        assert_eq!(trade.target_currency, Some("USD".into()));
    }

    #[test]
    fn serialize_convert_commit_request() {
        let req = ConvertCommitRequest {
            from_account: "USDC".into(),
            to_account: "USD".into(),
        };

        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["from_account"], "USDC");
        assert_eq!(json["to_account"], "USD");
    }
}
