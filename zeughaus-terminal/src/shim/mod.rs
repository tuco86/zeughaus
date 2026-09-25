//! Terminals that outlive the runner: one shim process per terminal owns
//! the PTY and the child, and a session talks to it over a Unix socket.
//!
//! A shim lives in `<root>/<terminal-id>/` with its `spec.json` (what to
//! start), its `sock` (where sessions connect) and its `shim.log` (its own
//! stderr). It keeps the last [`proto::REPLAY_BYTES`] of output, so a
//! session that attaches after a runner restart rebuilds the screen from
//! them. The shim ends only when a session sends `Close`; dropping a
//! session, or the whole runner, leaves it running.

pub mod client;
pub mod proto;
mod server;

pub use client::{ShimConn, ShimSender, close_dir};
pub use proto::{ShimExit, ShimSpec, Welcome};
pub use server::run;
