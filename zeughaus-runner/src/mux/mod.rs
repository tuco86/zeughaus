//! The terminal multiplexer this runner owns: the shared workspace, the
//! terminal sessions, and the `/mux` exchanges that serve them.
//!
//! `workspace` is the pure structure and its rules; `frames` reads and
//! writes `zeughaus_mux` frames on weida streams; `service` is everything
//! with a socket or a child process in it.

mod frames;
mod service;
mod workspace;

pub use service::MuxService;

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
