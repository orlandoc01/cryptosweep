//! Simple HTTP response cassette mechanism for smoke tests.
//!
//! Records raw JSON responses to fixture files on the first run, then
//! replays them on subsequent runs without hitting the network.
//!
//! Delete the cassette file to re-record.
//!
//! ## Single-response tests
//!
//! Use `replay_or_record` for tests that make one API call.
//!
//! ## Multi-response tests (e.g., quote → commit)
//!
//! Use `Cassette` to record/replay an ordered sequence of responses:
//!
//! ```rust,no_run
//! let mut cassette = Cassette::load_or_new("path/to/file.cassette.json");
//! let quote: QuoteResp = cassette.next_or_record(
//!     || async { client.post_raw("/convert/quote", &body).await },
//! ).await.unwrap();
//! // ... use quote ...
//! let commit: CommitResp = cassette.next_or_record(
//!     || async { client.post_raw("/convert/trade/123", &body).await },
//! ).await.unwrap();
//! cassette.save().unwrap();
//! ```

use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

/// Replays a cached JSON response from a cassette file, or records a new one
/// by executing the provided async closure.
///
/// For tests that make a single API call.
pub async fn replay_or_record<T, F, Fut>(cassette_path: &str, fetch: F) -> Result<T, String>
where
    T: DeserializeOwned,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let path = Path::new(cassette_path);

    // Replay: cassette file exists, deserialize from disk.
    if path.exists() {
        let contents =
            std::fs::read(path).map_err(|e| format!("Failed to read cassette: {e}"))?;
        let value: T = serde_json::from_slice(&contents)
            .map_err(|e| format!("Failed to deserialize cassette: {e}"))?;
        return Ok(value);
    }

    // Record: make the real call, save raw response to disk.
    let raw_bytes = fetch().await?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create cassette directory: {e}"))?;
    }

    let value: serde_json::Value = serde_json::from_slice(&raw_bytes)
        .map_err(|e| format!("Response is not valid JSON: {e}"))?;
    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("Failed to serialize: {e}"))?;
    std::fs::write(path, &pretty).map_err(|e| format!("Failed to write cassette: {e}"))?;

    let result: T = serde_json::from_value(value)
        .map_err(|e| format!("Failed to deserialize response: {e}"))?;
    Ok(result)
}

/// A multi-response cassette that records/replays an ordered sequence of
/// JSON responses. Used for tests that make multiple API calls.
///
/// The cassette file stores a JSON array of response objects. On replay,
/// responses are handed out in order via `next_or_record`.
pub struct Cassette {
    path: PathBuf,
    responses: Vec<serde_json::Value>,
    index: usize,
    /// True if we loaded from disk (replay mode).
    replaying: bool,
}

impl Cassette {
    /// Load an existing cassette file (replay mode) or start a new recording.
    pub fn load_or_new(cassette_path: &str) -> Self {
        let path = PathBuf::from(cassette_path);
        if path.exists() {
            let contents = std::fs::read_to_string(&path)
                .expect("Failed to read cassette file");
            let responses: Vec<serde_json::Value> = serde_json::from_str(&contents)
                .expect("Failed to parse cassette file as JSON array");
            Cassette {
                path,
                responses,
                index: 0,
                replaying: true,
            }
        } else {
            Cassette {
                path,
                responses: Vec::new(),
                index: 0,
                replaying: false,
            }
        }
    }

    /// Get the next response from the cassette (replay) or record a new one
    /// by calling the provided closure.
    pub async fn next_or_record<T, F, Fut>(&mut self, fetch: F) -> Result<T, String>
    where
        T: DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
    {
        if self.replaying {
            // Replay mode: return the next recorded response.
            let value = self
                .responses
                .get(self.index)
                .ok_or_else(|| {
                    format!(
                        "Cassette exhausted: expected response at index {} but only {} recorded",
                        self.index,
                        self.responses.len()
                    )
                })?
                .clone();
            self.index += 1;
            let result: T = serde_json::from_value(value)
                .map_err(|e| format!("Failed to deserialize cassette entry {}: {e}", self.index - 1))?;
            Ok(result)
        } else {
            // Record mode: make the real call and store the response.
            let raw_bytes = fetch().await?;
            let value: serde_json::Value = serde_json::from_slice(&raw_bytes)
                .map_err(|e| format!("Response is not valid JSON: {e}"))?;
            self.responses.push(value.clone());
            self.index += 1;
            let result: T = serde_json::from_value(value)
                .map_err(|e| format!("Failed to deserialize response: {e}"))?;
            Ok(result)
        }
    }

    /// Save the recorded cassette to disk. Call this at the end of a
    /// recording test run. No-op in replay mode.
    pub fn save(&self) -> Result<(), String> {
        if self.replaying {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create cassette directory: {e}"))?;
        }
        let pretty = serde_json::to_string_pretty(&self.responses)
            .map_err(|e| format!("Failed to serialize cassette: {e}"))?;
        std::fs::write(&self.path, &pretty)
            .map_err(|e| format!("Failed to write cassette: {e}"))?;
        Ok(())
    }
}
