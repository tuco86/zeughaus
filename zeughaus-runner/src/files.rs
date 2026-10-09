//! Small file helpers shared by the runner's persisted state: CI events and
//! pipelines, and the graph files.

use std::path::Path;

/// Writes `bytes` to `path` through a sibling `.tmp` file and a rename, so a
/// reader never sees half a file and a crash leaves the previous version.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// [`write_atomic`] of a value as pretty JSON.
pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(value).map_err(|e| format!("{}: {e}", path.display()))?;
    write_atomic(path, &text)
}

/// Reads a JSON file written by [`write_json`].
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", path.display()))
}
