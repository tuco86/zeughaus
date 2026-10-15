//! The editor's own face: ComicShannsMono Nerd Font Propo (Nerd Fonts
//! v3.5.1, licenses in `assets/fonts/`), the user's face, bundled so every
//! machine, the browser and a headless capture draw the same letters.
//!
//! It is the text face of the chrome and its icon set at once: the Propo
//! variant keeps the Nerd Font icons (codicons and the rest) at their own
//! width, about seven eighths of the font size, where the Mono variant the
//! terminal draws with squeezes them into one narrow cell.

use iced::Font;

/// The bytes to hand `iced::application(..).font(..)`.
pub const UI_BYTES: &[u8] =
    include_bytes!("../assets/fonts/ComicShannsMonoNerdFontPropo-Regular.otf");

/// The face of every label, title and icon of the chrome.
pub const UI: Font = Font::with_name("ComicShannsMono Nerd Font Propo");
