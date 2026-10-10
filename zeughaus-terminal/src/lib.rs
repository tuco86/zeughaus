//! The runner's terminal engine: one PTY, one parser and one canonical
//! screen per session, served as the mux wire model.
//!
//! This crate is where WezTerm's terminal lives and stops. It uses
//! `portable-pty` for the process boundary and a pinned `wezterm-term` for
//! escape parsing, modes, scrollback, stable rows and per-row damage -- the
//! parts of WezTerm that are a terminal -- and none of its mux, client,
//! codec, GUI, SSH, Lua or configuration stack. No WezTerm type crosses this
//! crate's public API: what comes out is [`zeughaus_mux`] rows, deltas and
//! heads, and what goes in is [`zeughaus_mux::TerminalCommand`].
//!
//! Everything a client can influence is bounded before it allocates: the grid
//! by [`zeughaus_mux::Dimensions::is_valid`], the history by
//! [`MAX_SCROLLBACK_ROWS`], a head's scrollback window by [`MAX_ROWS_ABOVE`],
//! a range fetch by [`MAX_FETCH_ROWS`], a paste by
//! [`zeughaus_mux::input::MAX_TEXT_BYTES`]. Malformed or hostile input is
//! refused, never panicked on.
//!
//! ```no_run
//! use zeughaus_mux::{Dimensions, TerminalId};
//! use zeughaus_terminal::wal::Wal;
//! use zeughaus_terminal::{Profile, Session, TerminalHost};
//!
//! let session = Session::spawn(
//!     TerminalId(1),
//!     &Profile::default_shell(),
//!     Dimensions { cols: 80, rows: 24 },
//!     Wal::Off,
//!     &TerminalHost::Local,
//! )?;
//! let head = session.head(0);
//! println!("{} rows at seq {}", head.rows.len(), head.seq);
//! # Ok::<(), zeughaus_terminal::SpawnError>(())
//! ```

mod config;
mod convert;
#[cfg(unix)]
pub mod locale;
mod model;
mod session;
#[cfg(unix)]
pub mod shim;
pub mod wal;

pub use config::MAX_SCROLLBACK_ROWS;
pub use model::{FIRST_EPOCH, MAX_FETCH_ROWS, MAX_ROWS_ABOVE};
#[cfg(unix)]
pub use session::ShimHost;
pub use session::{Profile, Session, SpawnError, TerminalHost};
