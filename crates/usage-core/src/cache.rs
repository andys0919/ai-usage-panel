//! The last result, kept on disk so a restart shows numbers immediately and does not
//! knock on rate-limited official endpoints again. Holds no secrets (only `AccountView`s).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;
use crate::model::AccountView;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    /// When the last full collection cycle started (epoch ms).
    pub cycle_started_ms: Option<i64>,
    pub accounts: Vec<AccountView>,
}

pub fn load(path: &Path) -> Option<Cache> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save(path: &Path, cache: &Cache) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes =
        serde_json::to_vec(cache).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_atomic(path, &bytes)
}
