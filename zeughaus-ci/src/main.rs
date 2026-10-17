//! zeughaus-ci: pipelines a repository defines as scripts in its
//! `.zeughaus-ci/` folder, run on the runners that enlist into this process.

mod artifacts;
mod channels;
mod cleanup;
mod cli;
mod config;
mod enlist;
mod event;
mod excerpt;
mod files;
mod forge;
mod header;
mod hook;
mod pipeline;
mod runners;
mod scheduler;
mod secrets;
mod serve;
mod service;
mod streak;
mod transcripts;
mod transport;
mod tryrun;

use std::path::{Path, PathBuf};

/// The folder of a repository that holds its CI jobs.
pub const CI_DIR: &str = ".zeughaus-ci";

/// Seconds since the unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The `ci/` directory under the state directory.
pub fn ci_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("ci")
}

fn main() -> std::process::ExitCode {
    cli::main()
}
