//! CI: pipelines a repository defines as scripts in its `.ci/` folder, run
//! as this runner's jobs.
//!
//! Events (a webhook, `ci run`, a cron schedule) land in `ci/inbox/` as files,
//! are coalesced per ref into `ci/queue/`, and the scheduler thread turns a
//! queued event into a pipeline: the jobs its `.ci` headers select, each a
//! run of the [`JobHost`](crate::jobs::JobHost) on the host, in a rootless
//! podman container, or in a managed VM. Everything the scheduler knows is on
//! disk under `<state-dir>/ci/`, so a restarted runner picks up where the
//! previous one stopped.
//!
//! Without `<state-dir>/ci.toml` none of this runs and the runner is exactly
//! what it is without CI.

pub mod busy;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod event;
pub mod forge;
pub mod header;
pub mod hook;
pub mod launch;
pub mod machine;
pub mod pipeline;
pub mod scheduler;

use std::path::Path;

pub use scheduler::start;

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

/// Seconds since the unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The `ci/` directory under the state directory.
pub fn ci_dir(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("ci")
}
