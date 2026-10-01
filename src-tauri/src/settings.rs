use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use usage_core::fsutil::write_atomic;

/// The official endpoints rate-limit, so the interval can never be shorter than 5 minutes.
pub const ALLOWED_INTERVALS_SECS: [u64; 4] = [300, 600, 900, 1800];
pub const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Minimum time between two manual refreshes.
pub const MANUAL_COOLDOWN_MS: i64 = 60_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub interval_secs: u64,
    /// Refresh expired OAuth tokens (and write them back to the credential file).
    pub auto_refresh_tokens: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval_secs: DEFAULT_INTERVAL_SECS,
            auto_refresh_tokens: true,
        }
    }
}

impl Settings {
    fn sanitized(mut self) -> Self {
        if !ALLOWED_INTERVALS_SECS.contains(&self.interval_secs) {
            self.interval_secs = DEFAULT_INTERVAL_SECS;
        }
        self
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .unwrap_or_default()
            .sanitized()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_atomic(path, &bytes)
    }
}
