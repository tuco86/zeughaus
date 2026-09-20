//! An iced widget that draws a [`zeughaus_mux::view::TerminalView`].
//!
//! The runner owns the terminal; this crate owns the pixels and the input.
//! One [`Terminal`] per surface renders one custom `wgpu` primitive -- not a
//! widget per cell, not a text object per row -- and reports what the user did
//! as an [`Action`] the application turns into a
//! [`zeughaus_mux::TerminalCommand`] or applies to its own view.
//!
//! ```no_run
//! # use zeughaus_mux::view::TerminalView;
//! # fn demo<Message: Clone>(view: &TerminalView, wrap: impl Fn(iced_terminal::Action) -> Message + 'static) {
//! let terminal = iced_terminal::Terminal::new(view, 1)
//!     .controlling(true)
//!     .focused(true)
//!     .next_serial(42)
//!     .on_action(wrap);
//! # }
//! ```
//!
//! The application must register [`font_bytes`] with
//! `iced::application(..).font(..)` for the bundled face to be available to
//! the rest of the interface; the widget registers it for itself either way.
//!
//! Two things this widget deliberately never does, both of them security
//! decisions of the plan rather than omissions: it never opens a hyperlink
//! (it reports [`Action::OpenLink`] on an explicit `Ctrl`+click and lets the
//! application decide), and it never reads or writes the clipboard on the
//! terminal's behalf -- there is no OSC 52 path in or out of here.

mod cache;
mod font;
mod geometry;
mod input;
mod pipeline;
mod widget;

pub mod selection;

pub use font::{FAMILY, FONT, FONT_BOLD, cell_geometry, font_bytes};
pub use geometry::{CellMetrics, cell_at, grid_size};
pub use input::{key_input, modifiers, mouse_button};
pub use widget::{Action, Terminal};
