//! Job plugin: a process with a beginning and an end, as a node.
//!
//! A job is a graph node because that is where its configuration already
//! belongs: what to start, with which arguments, environment and working
//! directory, and which files to keep afterwards. The runner executes it in
//! the mux it already owns, so a failed job is a terminal to attach to rather
//! than a log to read -- and that is exactly why the mux stays argv-free: no
//! `TopologyCommand` ever carries a command line, because the command line
//! lives in this node's settings and reaches the runner as a graph change.
//!
//! The plugin does not know how to start anything. It declares
//! [`ProcessHost`], and the runner implements it over its mux service. The
//! editor registers the plugin *detached* ([`JobPlugin::detached`]): it needs
//! the catalog entry to draw and edit a job node, and it never executes one,
//! so a node created there refuses to run instead of starting a process on a
//! machine that is only supposed to draw.

mod node;

pub use node::JobNode;

use std::path::PathBuf;
use std::sync::Arc;

use zeughaus_core::{DomainPlugin, ExecutableNode, NodeDefinition, catalog_entry};

/// Everything the host needs to start one run, resolved from the node's
/// settings. The node owns the parsing; the host owns the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// What the run's terminal is called until the child names itself.
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    /// Added to the runner's own environment.
    pub env: Vec<(String, String)>,
    /// `None` runs the child in the runner's working directory.
    pub cwd: Option<PathBuf>,
    /// The directory this run's log, exit record and artifacts go into,
    /// already created by [`ProcessHost::new_run_dir`].
    pub run_dir: PathBuf,
    /// After a non-zero exit, leave a shell in the run's terminal with the
    /// same working directory and environment, so the failure can be looked
    /// at where it happened. A host that cannot do that ignores it.
    pub keep_on_failure: bool,
}

/// How a run ended.
///
/// `code` is `None` when no exit code exists -- the child was killed or died
/// from a signal -- which is why `killed` is carried beside it rather than
/// folded into a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunExit {
    pub code: Option<i32>,
    pub killed: bool,
}

/// A started run, waited for on a background thread.
///
/// Consuming (`self: Box<Self>`) because waiting happens exactly once: the
/// deferred work that holds the handle is the only thing that may observe the
/// end of the run.
pub trait RunHandle: Send {
    fn wait(self: Box<Self>) -> RunExit;
}

/// What the plugin needs from the process that executes.
///
/// Implemented by the runner over its mux service; absent in the editor.
pub trait ProcessHost: Send + Sync {
    /// Whether the runner is held: it starts no new run and lets the live
    /// ones finish.
    fn held(&self) -> bool;

    /// Allocate `<state-dir>/runs/<run-id>/`, created, and return it.
    fn new_run_dir(&self) -> Result<PathBuf, String>;

    /// Start the process in an owned terminal, teeing its PTY bytes to
    /// `<run_dir>/log`.
    fn spawn(&self, spec: JobSpec) -> Result<Box<dyn RunHandle>, String>;
}

/// The `job` domain: one node type, and the host it runs on.
///
/// `host` is `None` in a process that does not execute; see the module
/// documentation.
pub struct JobPlugin {
    host: Option<Arc<dyn ProcessHost>>,
}

impl JobPlugin {
    pub fn new(host: Arc<dyn ProcessHost>) -> Self {
        Self { host: Some(host) }
    }

    /// The catalog without the ability to execute: what an editor registers.
    pub fn detached() -> Self {
        Self { host: None }
    }
}

impl DomainPlugin for JobPlugin {
    fn name(&self) -> &str {
        "job"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![catalog_entry("job.run", "Job", "Job", &JobNode::new(None))]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "job.run" => Some(Box::new(JobNode::new(self.host.clone()))),
            _ => None,
        }
    }
}
