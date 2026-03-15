//! Crypto Sweep — Automated USDC deposit detection and Coinbase liquidation.
//!
//! Cron-based entrypoint: acquires a lock, loads config and state,
//! runs the deposit check pass, persists state, and exits.

mod cbworker;
mod coinbase;
mod config;
mod explorer;
mod notifier;
mod orchestrator;
mod persistence;
#[cfg(test)]
mod tests;
mod types;

use std::collections::HashMap;
use std::process;

use tracing::{error, info, warn};

use crate::coinbase::LiveCoinbaseClient;
use crate::config::CryptoSweepConfig;
use crate::explorer::LiveBlockExplorer;
use crate::notifier::TelegramNotifier;
use crate::types::{AppError, Chain, Notifier};
use strum::IntoEnumIterator;

/// Default path to the lock file.
const LOCK_PATH: &str = "/tmp/cryptosweep.lock";

#[tokio::main]
async fn main() {
    // Initialize structured logging (logs to stdout, picked up by cron redirect).
    tracing_subscriber::fmt::init();

    info!("cryptosweep starting");

    // 1. Load config (needed early so we can notify on lock failure).
    let config_path = config::default_config_path();
    let config = match CryptoSweepConfig::load(&config_path) {
        Ok(c) => {
            info!(?config_path, "Configuration loaded");
            c
        }
        Err(e) => {
            error!("Failed to load configuration: {e}");
            process::exit(1);
        }
    };

    // 2. Set up Telegram notifier early so we can alert on lock/state failures.
    let notifier = match TelegramNotifier::new(&config.telegram_bot_token, &config.telegram_chat_id) {
        Ok(n) => n,
        Err(e) => {
            error!("Invalid Telegram config: {e}");
            process::exit(1);
        }
    };

    // 3. Acquire lock file — notify and exit if another instance is running.
    let _lock = match persistence::acquire_lock(LOCK_PATH.as_ref()) {
        Ok(lock) => lock,
        Err(e) => {
            warn!("Cannot acquire lock, another instance may be running: {e}");
            _ = notifier.notify(&format!("cryptosweep: failed to acquire lock: {e}")).await;
            process::exit(0);
        }
    };

    // 4. Run the main execution flow. Errors are reported via Telegram.
    if let Err(e) = run(&config, &notifier).await {
        error!(%e, "Fatal error during run");
        _ = notifier.notify(&format!("cryptosweep fatal: {e}")).await;
        process::exit(1);
    }

    info!("cryptosweep run complete");
    // Lock is released when `_lock` is dropped here.
}

/// Main execution flow: load state, run deposit check, prune, persist, notify.
///
/// Separated from `main` so fallible operations can use `?` instead of
/// match/exit. The caller handles fatal errors and Telegram notification.
async fn run(config: &CryptoSweepConfig, notifier: &TelegramNotifier) -> Result<(), AppError> {
    let mut state = persistence::load_state(&config.state_file)?;

    let coinbase = LiveCoinbaseClient::new(
        config.coinbase_api_key.clone(),
        &config.coinbase_api_secret,
        config.coinbase_usdc_account_id.clone(),
        config.coinbase_usd_account_id.clone(),
    )?;
    let explorers: HashMap<Chain, LiveBlockExplorer> = Chain::iter()
        .map(|c| (c, LiveBlockExplorer::new(c)))
        .collect();

    // Deposit check pass.
    let pass = orchestrator::DepositCheck::new(
        config, &mut state, &explorers, &coinbase, notifier,
    );
    let mut errors: Vec<String> = pass.run().await;

    // Prune entries older than 90 days.
    persistence::prune_stale_entries(&mut state);

    // Persist state.
    if let Err(e) = persistence::save_state(&config.state_file, &state) {
        error!("Failed to save state: {e}");
        errors.push(format!("State save: {e}"));
    }

    // Send Telegram alert if any errors occurred during this run.
    if !errors.is_empty() {
        let msg = format!(
            "cryptosweep run errors ({})\n\n{}",
            errors.len(),
            errors.join("\n"),
        );
        if let Err(e) = notifier.notify(&msg).await {
            error!(%e, "Failed to send error notification");
        }
    }

    Ok(())
}
