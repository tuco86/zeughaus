//! An iced widget that draws a [`zeughaus_mux::view::TerminalView`].
//!
//! The runner owns the terminal; this crate owns the pixels and the input.
//! The view itself lives behind a [`SharedView`] handle the transport task
//! writes and the widget reads under a short lock, so a head or a delta
//! arriving never has to travel through the application's message loop.
//! One [`Terminal`] per surface renders one custom `wgpu` primitive -- not a
//! widget per cell, not a text object per row -- and reports what the user did
//! as an [`Action`] the application turns into a
//! [`zeughaus_mux::TerminalCommand`] or applies to its own view.
//!
//! ```no_run
//! # use std::sync::{Arc, Mutex};
//! # use zeughaus_mux::view::TerminalView;
//! # fn demo<Message: Clone>(view: TerminalView, wrap: impl Fn(iced_terminal::Action) -> Message + 'static) {
//! let view: iced_terminal::SharedView = Arc::new(Mutex::new(Some(view)));
//! let terminal: iced_terminal::Terminal<'_, Message> =
//!     iced_terminal::Terminal::new(view.clone(), 1)
//!         .controlling(true)
//!         .focused(true)
//!         .next_serial(42)
//!         .on_action(wrap);
//! # }
//! ```
//!
//! The application must register [`font_bytes`] with
//! `iced::application(..).font(..)` for the bundled face to be available to
//! the rest of the interface; the widget registers it for itself either way.
//!
//! Colours come from the host's theme through [`Catalog`]: a [`Style`] states
//! the palette a terminal that changed nothing is drawn with, and the
//! selection highlight. What the child set through `OSC 4`/`10`/`11` is left
//! alone -- [`zeughaus_mux::Palette::themed`] merges the two -- so a theme
//! switch recolours the untouched slots and reshapes no row.
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
mod style;
mod widget;

pub mod selection;

pub use font::{FAMILY, FONT, FONT_BOLD, cell_geometry, font_bytes};
pub use geometry::{CellMetrics, cell_at, grid_size};
pub use input::{AltSide, Platform, key_input, modifiers, mouse_button};
pub use style::{Catalog, Style, StyleFn, default};
pub use widget::{Action, SharedView, Terminal};

/// The palette a terminal states its colours in. A host's theme builds one
/// to fill what the child left at its defaults.
pub use zeughaus_mux::Palette;
