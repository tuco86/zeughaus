//! The terminal multiplexer this runner owns: the shared workspace, the
//! terminal sessions, and the `/mux` exchanges that serve them.
//!
//! `workspace` is the pure structure and its rules; `frames` reads and
//! writes `zeughaus_mux` frames on weida streams; `service` is everything
//! with a socket or a child process in it.

mod frames;
mod persist;
mod service;
mod workspace;

use std::collections::{HashMap, HashSet};

pub use persist::SavedRun;
pub use service::MuxService;

/// Where a terminal the runner starts for itself appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedPlacement {
    /// In no pane: listed as detached until a client attaches it.
    Detached,
    /// In a tab of its own inside the runner's locked group: the run of a
    /// press from outside any editor, which nobody asked to see and so
    /// nobody would find among the detached terminals.
    Triggered,
}

/// What the store says about the graphs a pane may show, as the runner
/// mirrors it.
#[derive(Debug, Clone, Default)]
pub struct GraphSync {
    /// The top-level graphs this runner executes, in id order.
    pub owned: Vec<u64>,
    /// Every node's display name: any container may be open in a pane.
    pub names: HashMap<u64, String>,
    /// Every node id the store holds.
    pub exists: HashSet<u64>,
}

/// A fresh incarnation for this process.
///
/// Sixteen random bytes with no crate to draw them from: `RandomState` is
/// seeded from the OS per instance, so hashing a constant through two of
/// them is two independent 64-bit draws. Not a key -- it names a process
/// run, and only has to differ from every other run an editor may have
/// cached.
pub fn incarnation() -> zeughaus_mux::RunnerIncarnation {
    use std::hash::{BuildHasher, RandomState};
    let a = RandomState::new().hash_one(0u8).to_le_bytes();
    let b = RandomState::new().hash_one(1u8).to_le_bytes();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a);
    bytes[8..].copy_from_slice(&b);
    zeughaus_mux::RunnerIncarnation::from_bytes(bytes)
}
