//! State persistence: load and save `PersistedState` to a JSON file.
//!
//! Also provides file-lock acquisition via `fs2` to prevent overlapping
//! cron runs. The lock is held for the lifetime of the returned `File`
//! handle — dropping the handle releases the lock.

use std::fs::{self, File, OpenOptions};
use std::path::Path;

use fs2::FileExt;
use tracing::info;

use crate::types::{AppError, PersistedState};

/// Load persisted state from a JSON file.
///
/// If the file doesn't exist, returns `PersistedState::default()` (empty
/// state). This handles first-run gracefully — no manual setup needed.
///
/// If the file exists but contains invalid JSON, returns an error.
pub fn load_state(path: &Path) -> Result<PersistedState, AppError> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let state: PersistedState = serde_json::from_str(&contents)?;
            info!(
                bank_payments = state.bank_payments.len(),
                processed_tx_hashes = state.processed_tx_hashes.len(),
                "Loaded persisted state"
            );
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!(?path, "State file not found, starting with empty state");
            Ok(PersistedState::default())
        }
        Err(e) => Err(e.into()),
    }
}

/// Save persisted state to a JSON file.
///
/// Creates parent directories if they don't exist. Writes pretty-printed
/// JSON for human auditability (the state file is small).
///
/// Uses write-to-temp-then-rename for atomic updates: if the process is
/// killed mid-write, the original state file remains intact.
pub fn save_state(path: &Path, state: &PersistedState) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(state)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    info!(?path, "Persisted state saved");
    Ok(())
}

/// Remove BankPayment entries older than 90 days.
///
/// BankPayments are aged by `processed_at`.
/// When a BankPayment is pruned, its `deposit_tx_hash` is also removed
/// from `processed_tx_hashes` — safe because the on-chain tx is far too
/// old to reappear in a block explorer query bounded by `last_seen_block`.
pub fn prune_stale_entries(state: &mut PersistedState) {
    use chrono::{Duration, Utc};

    let cutoff = Utc::now() - Duration::days(90);
    let before_payments = state.bank_payments.len();

    // Collect tx hashes to remove from the dedup set.
    let stale_tx_hashes: Vec<String> = state
        .bank_payments
        .iter()
        .filter(|(_id, p)| p.processed_at < cutoff)
        .map(|(_id, p)| p.deposit_tx_hash.clone())
        .collect();

    state.bank_payments.retain(|_id, p| p.processed_at >= cutoff);

    for hash in &stale_tx_hashes {
        state.processed_tx_hashes.remove(hash);
    }

    let pruned_payments = before_payments - state.bank_payments.len();

    if pruned_payments > 0 {
        info!(
            pruned_payments,
            pruned_tx_hashes = stale_tx_hashes.len(),
            "Pruned stale entries (>90 days)"
        );
    }
}

/// Acquire an exclusive file lock to prevent overlapping cron runs.
///
/// Returns the locked `File` handle. The lock is released when the handle
/// is dropped (end of `main`). Uses `fs2::try_lock_exclusive()` which
/// returns immediately if the lock is already held — no blocking.
///
/// If the lock file doesn't exist, it's created. The lock file contains
/// no meaningful data; only the OS-level advisory lock matters.
pub fn acquire_lock(path: &Path) -> Result<File, AppError> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    file.try_lock_exclusive()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BankPayment, BankPaymentOutcome, Chain};
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use std::io::Write;
    use tempfile::TempDir;

    /// Helper: create a state with one payment for tests.
    fn sample_state() -> PersistedState {
        let now = Utc::now();
        let tx_hash = "0xabc12345".to_string();

        let payment = BankPayment {
            id: BankPayment::make_id(&now, &tx_hash),
            deposit_address: "0xDeposit".to_string(),
            chain: Chain::Base,
            deposit_tx_hash: tx_hash.clone(),
            usdc_amount: dec!(2400.00),
            payment_method_id: "a1b2c3d4-uuid".to_string(),
            outcome: BankPaymentOutcome::Initiated {
                sell_order_id: "order-1".to_string(),
                usd_proceeds: dec!(2399.50),
                withdrawal_transfer_id: "wd-1".to_string(),
                initiated_at: now,
            },
            deposit_timestamp: now,
            processed_at: now,
        };

        let mut state = PersistedState::default();
        state.bank_payments.insert(payment.id.clone(), payment);
        state.processed_tx_hashes.insert("0xabc12345".to_string());
        state
    }

    #[test]
    fn round_trip_save_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");

        let original = sample_state();
        save_state(&path, &original).expect("save");
        let loaded = load_state(&path).expect("load");

        assert_eq!(loaded, original);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");

        let state = load_state(&path).expect("should return default");
        assert!(state.bank_payments.is_empty());
        assert!(state.processed_tx_hashes.is_empty());
    }

    #[test]
    fn load_invalid_json_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.json");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"not valid json!!!").unwrap();

        let result = load_state(&path);
        assert!(result.is_err());
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("dirs").join("state.json");

        let state = PersistedState::default();
        save_state(&path, &state).expect("should create parents");
        assert!(path.exists());
    }

    #[test]
    fn acquire_lock_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.lock");

        let _lock = acquire_lock(&path).expect("should acquire lock");
        assert!(path.exists());
    }

    #[test]
    fn acquire_lock_fails_if_already_held() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.lock");

        let _first = acquire_lock(&path).expect("first lock");
        let second = acquire_lock(&path);
        assert!(second.is_err());
    }

    // -- Prune tests --

    #[test]
    fn prune_removes_old_entries() {
        let mut state = PersistedState::default();
        let now = Utc::now();
        let old = now - chrono::Duration::days(91);

        // Old BankPayment (should be pruned along with its tx hash).
        let old_payment = BankPayment {
            id: BankPayment::make_id(&old, "0xoldtx1234"),
            deposit_address: "0xAddr".into(),
            chain: Chain::Base,
            deposit_tx_hash: "0xoldtx1234".into(),
            usdc_amount: dec!(100.00),
            payment_method_id: "pm-1".into(),
            outcome: BankPaymentOutcome::Initiated {
                sell_order_id: "o-1".into(),
                usd_proceeds: dec!(99.50),
                withdrawal_transfer_id: "wd-1".into(),
                initiated_at: old,
            },
            deposit_timestamp: old,
            processed_at: old,
        };
        state
            .bank_payments
            .insert(old_payment.id.clone(), old_payment);
        state
            .processed_tx_hashes
            .insert("0xoldtx1234".to_string());

        // Recent BankPayment (should survive).
        let new_payment = BankPayment {
            id: BankPayment::make_id(&now, "0xnewtx5678"),
            deposit_address: "0xAddr".into(),
            chain: Chain::Base,
            deposit_tx_hash: "0xnewtx5678".into(),
            usdc_amount: dec!(200.00),
            payment_method_id: "pm-1".into(),
            outcome: BankPaymentOutcome::Initiated {
                sell_order_id: "o-2".into(),
                usd_proceeds: dec!(199.50),
                withdrawal_transfer_id: "wd-2".into(),
                initiated_at: now,
            },
            deposit_timestamp: now,
            processed_at: now,
        };
        let new_payment_id = new_payment.id.clone();
        state
            .bank_payments
            .insert(new_payment.id.clone(), new_payment);
        state
            .processed_tx_hashes
            .insert("0xnewtx5678".to_string());

        prune_stale_entries(&mut state);

        assert_eq!(state.bank_payments.len(), 1);
        assert!(state.bank_payments.contains_key(&new_payment_id));

        assert!(!state.processed_tx_hashes.contains("0xoldtx1234"));
        assert!(state.processed_tx_hashes.contains("0xnewtx5678"));
    }

    #[test]
    fn prune_no_op_when_all_recent() {
        let mut state = PersistedState::default();
        let now = Utc::now();

        let payment = BankPayment {
            id: BankPayment::make_id(&now, "0xnewtx5678"),
            deposit_address: "0xAddr".into(),
            chain: Chain::Base,
            deposit_tx_hash: "0xnewtx5678".into(),
            usdc_amount: dec!(200.00),
            payment_method_id: "pm-1".into(),
            outcome: BankPaymentOutcome::Initiated {
                sell_order_id: "o-1".into(),
                usd_proceeds: dec!(199.50),
                withdrawal_transfer_id: "wd-1".into(),
                initiated_at: now,
            },
            deposit_timestamp: now,
            processed_at: now,
        };
        state.bank_payments.insert(payment.id.clone(), payment);

        prune_stale_entries(&mut state);

        assert_eq!(state.bank_payments.len(), 1);
    }
}
