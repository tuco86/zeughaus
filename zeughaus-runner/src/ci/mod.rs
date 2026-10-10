//! CI: pipelines a repository defines as scripts in its `.zeughaus-ci/` folder, run
//! as this runner's jobs.
//!
//! Events (a webhook, `ci run`, a cron schedule) land in `ci/inbox/` as files,
//! are coalesced per ref into `ci/queue/`, and the scheduler thread turns a
//! queued event into a pipeline: the jobs its `.zeughaus-ci` headers select, each a
//! run of the [`JobHost`](crate::jobs::JobHost) on the host, in a rootless
//! podman container, or in a managed VM. Everything the scheduler knows is on
//! disk under `<state-dir>/ci/`, so a restarted runner picks up where the
//! previous one stopped.
//!
//! Editors read the channels, pipelines and machines over `/ci` ([`serve`])
//! and open a job's output as a read-only terminal; the scheduler announces
//! every record it writes, so an editor asks again only when one changed.
//!
//! Without `<state-dir>/ci.toml` none of this runs and the runner is exactly
//! what it is without CI.

pub mod busy;
pub mod channels;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod event;
pub mod excerpt;
pub mod forge;
pub mod header;
pub mod hook;
pub mod launch;
pub mod machine;
pub mod pipeline;
pub mod replay;
pub mod scheduler;
pub mod secrets;
pub mod serve;
pub mod streak;

use std::path::Path;

pub use scheduler::{Ci, start};

/// The folder of a repository that holds its CI jobs.
pub const CI_DIR: &str = ".zeughaus-ci";

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
