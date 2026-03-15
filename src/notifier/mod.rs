//! Notifier module: one-way Telegram alerts.
//!
//! Implements the `Notifier` trait from `types.rs` using the teloxide crate.
//! Sends generic text alerts for completed actions, errors, etc.

use teloxide::prelude::*;
use teloxide::types::Recipient;
use tracing::{error, info};

use crate::types::{Notifier, AppError};

/// Live Telegram notifier. Sends messages via the Telegram Bot API.
///
/// Holds a `teloxide::Bot` instance and the target chat ID. The bot token
/// and chat ID come from `CryptoSweepConfig`.
pub struct TelegramNotifier {
    bot: Bot,
    chat_id: Recipient,
}

impl TelegramNotifier {
    /// Create a new notifier with the given bot token and chat ID.
    ///
    /// Returns `AppError::Config` if `chat_id` is not a valid `i64`.
    pub fn new(bot_token: &str, chat_id: &str) -> Result<Self, AppError> {
        let parsed_id = chat_id.parse::<i64>().map_err(|e| {
            AppError::Config(format!("telegram_chat_id must be a valid i64: {e}"))
        })?;

        Ok(Self {
            bot: Bot::new(bot_token),
            chat_id: Recipient::Id(ChatId(parsed_id)),
        })
    }
}

impl Notifier for TelegramNotifier {
    /// Send a plain text message to the configured Telegram chat.
    async fn notify(&self, message: &str) -> Result<(), AppError> {
        info!(message, "Sending Telegram notification");

        self.bot
            .send_message(self.chat_id.clone(), message)
            .await
            .map_err(|e| {
                error!(%e, "Failed to send Telegram message");
                AppError::Notification(format!("Telegram send failed: {e}"))
            })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_with_valid_chat_id() {
        let notifier = TelegramNotifier::new("fake-token", "-1001234567890");
        assert!(notifier.is_ok());
    }

    #[test]
    fn new_with_invalid_chat_id_returns_config_error() {
        let result = TelegramNotifier::new("fake-token", "not-a-number");
        assert!(result.is_err());
        assert!(matches!(result, Err(AppError::Config(_))));
    }
}
