//! The seam between the wire model and WezTerm's types.
//!
//! Everything crossing it is small and total: a [`KeyInput`] becomes a
//! `KeyCode` plus modifiers and the terminal decides what bytes that is, a
//! `CellAttributes` becomes a [`CellStyle`] of wire-stable flags and colours.
//! Nothing here allocates per cell except a span's text, and nothing here can
//! fail: a client that sends nonsense gets a key the terminal ignores, not an
//! error path.
//!
//! The direction matters. Keys travel in as *semantics* (`Ctrl` plus the
//! character `c`) and are encoded where the terminal's modes are known --
//! application cursor keys, `modifyOtherKeys`, newline mode, kitty's
//! keyboard flags -- which is what `key_down` does for the legacy encodings
//! and [`kitty`] for the one `wezterm-term` leaves to its embedder. Cells
//! travel out as *appearance* and are resolved against the palette on the
//! client, so an OSC 4 repaints without resending a single row.

use termwiz::surface::{CursorShape as WezCursorShape, CursorVisibility};
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{
    Blink, CellAttributes, CursorPosition, Intensity, KeyCode, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind, Terminal,
};
use wezterm_term::{Underline as WezUnderline, color::SrgbaTuple};

use wezterm_input_types::{KeyCode as KittyCode, KeyEvent, KeyboardLedStatus, KittyKeyboardFlags};
use zeughaus_mux::input::{Key, KeyKind, MouseButton as WireMouseButton, NamedKey};
use zeughaus_mux::{
    CellStyle, Cursor, CursorShape, KeyInput, Modes, Modifiers, MouseInput, MouseKind, Palette,
    StyleFlags, Underline, WireColor,
};

/// A semantic keystroke as the terminal core wants it, for the legacy
/// encodings; `None` for a release, which only kitty's protocol reports.
///
/// The named keys that WezTerm deliberately has no variant for (it documents
/// each of them in `KeyCode`) are their control characters: `Enter` is a
/// carriage return and newline mode decides whether that becomes CRLF,
/// `Backspace` is `0x08`, `Delete` is `0x7f`.
///
/// `Shift+Enter` is a line feed. xterm has no encoding for it and would send
/// the carriage return plain `Enter` sends, which submits a prompt instead of
/// breaking the line; line-oriented programs read LF as CR, and editors that
/// tell them apart (Claude Code, omp: `Ctrl+J`) insert a newline.
pub(crate) fn key(input: KeyInput) -> Option<(KeyCode, KeyModifiers)> {
    if input.kind == KeyKind::Release {
        return None;
    }
    if input.key == Key::Named(NamedKey::Enter) && input.modifiers == Modifiers(Modifiers::SHIFT) {
        return Some((KeyCode::Char('\n'), KeyModifiers::NONE));
    }
    let code = match input.key {
        Key::Char(c) => KeyCode::Char(c),
        Key::Named(named) => match named {
            NamedKey::Enter => KeyCode::Char('\r'),
            NamedKey::Tab => KeyCode::Char('\t'),
            NamedKey::Backspace => KeyCode::Char('\u{8}'),
            NamedKey::Escape => KeyCode::Char('\u{1b}'),
            NamedKey::Delete => KeyCode::Char('\u{7f}'),
            NamedKey::Insert => KeyCode::Insert,
            NamedKey::Home => KeyCode::Home,
            NamedKey::End => KeyCode::End,
            NamedKey::PageUp => KeyCode::PageUp,
            NamedKey::PageDown => KeyCode::PageDown,
            NamedKey::Up => KeyCode::UpArrow,
            NamedKey::Down => KeyCode::DownArrow,
            NamedKey::Left => KeyCode::LeftArrow,
            NamedKey::Right => KeyCode::RightArrow,
            NamedKey::F(n) => KeyCode::Function(n),
        },
    };
    Some((code, modifiers(input.modifiers)))
}

/// A keystroke in kitty's keyboard protocol, for a child that pushed
/// `flags`: the encoder WezTerm's own GUI uses, fed the same semantics as the
/// legacy path (control characters for the keys it names that way). Empty
/// when the flags ask for nothing, such as a release without event types.
///
/// There is no raw key event behind a wire key, so what needs one is not
/// reported: a bare modifier key, and the base-layout key of
/// `REPORT_ALTERNATE_KEYS` (the shifted key is, from the character itself).
pub(crate) fn kitty(input: KeyInput, flags: KittyKeyboardFlags) -> String {
    let key = match input.key {
        Key::Char(c) => KittyCode::Char(c),
        Key::Named(named) => match named {
            NamedKey::Enter => KittyCode::Char('\r'),
            NamedKey::Tab => KittyCode::Char('\t'),
            NamedKey::Backspace => KittyCode::Char('\u{8}'),
            NamedKey::Escape => KittyCode::Char('\u{1b}'),
            NamedKey::Delete => KittyCode::Char('\u{7f}'),
            NamedKey::Insert => KittyCode::Insert,
            NamedKey::Home => KittyCode::Home,
            NamedKey::End => KittyCode::End,
            NamedKey::PageUp => KittyCode::PageUp,
            NamedKey::PageDown => KittyCode::PageDown,
            NamedKey::Up => KittyCode::UpArrow,
            NamedKey::Down => KittyCode::DownArrow,
            NamedKey::Left => KittyCode::LeftArrow,
            NamedKey::Right => KittyCode::RightArrow,
            NamedKey::F(n) => KittyCode::Function(n),
        },
    };
    KeyEvent {
        key,
        modifiers: modifiers(input.modifiers),
        leds: KeyboardLedStatus::empty(),
        repeat_count: 1,
        key_is_down: input.kind == KeyKind::Press,
        raw: None,
        #[cfg(windows)]
        win32_uni_char: None,
    }
    .encode_kitty(flags)
}

pub(crate) fn modifiers(mods: Modifiers) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    if mods.has(Modifiers::SHIFT) {
        out |= KeyModifiers::SHIFT;
    }
    if mods.has(Modifiers::CTRL) {
        out |= KeyModifiers::CTRL;
    }
    if mods.has(Modifiers::ALT) {
        out |= KeyModifiers::ALT;
    }
    if mods.has(Modifiers::SUPER) {
        out |= KeyModifiers::SUPER;
    }
    out
}

/// A mouse event in grid coordinates. Pixel offsets are zero: the client
/// reports cells, and the pixel-precision mouse encodings would need a font
/// the runner does not have.
pub(crate) fn mouse(input: MouseInput) -> MouseEvent {
    let kind = match input.kind {
        MouseKind::Press => MouseEventKind::Press,
        MouseKind::Release => MouseEventKind::Release,
        MouseKind::Move => MouseEventKind::Move,
    };
    let button = match input.button {
        Some(WireMouseButton::Left) => MouseButton::Left,
        Some(WireMouseButton::Middle) => MouseButton::Middle,
        Some(WireMouseButton::Right) => MouseButton::Right,
        // One notch per event: the client sends one input per detent, so the
        // runner never has to guess how far a trackpad meant to go.
        Some(WireMouseButton::WheelUp) => MouseButton::WheelUp(1),
        Some(WireMouseButton::WheelDown) => MouseButton::WheelDown(1),
        None => MouseButton::None,
    };
    MouseEvent {
        kind,
        x: input.col as usize,
        y: input.row as i64,
        x_pixel_offset: 0,
        y_pixel_offset: 0,
        button,
        modifiers: modifiers(input.modifiers),
    }
}

/// The style of one cell, as one comparable word plus three colours.
pub(crate) fn style(attrs: &CellAttributes) -> CellStyle {
    let mut flags = StyleFlags::default();
    match attrs.intensity() {
        Intensity::Bold => flags = flags.with(StyleFlags::BOLD),
        Intensity::Half => flags = flags.with(StyleFlags::DIM),
        Intensity::Normal => {}
    }
    if attrs.blink() != Blink::None {
        flags = flags.with(StyleFlags::BLINK);
    }
    if attrs.italic() {
        flags = flags.with(StyleFlags::ITALIC);
    }
    if attrs.reverse() {
        flags = flags.with(StyleFlags::REVERSE);
    }
    if attrs.strikethrough() {
        flags = flags.with(StyleFlags::STRIKETHROUGH);
    }
    if attrs.invisible() {
        flags = flags.with(StyleFlags::INVISIBLE);
    }
    if attrs.overline() {
        flags = flags.with(StyleFlags::OVERLINE);
    }
    flags = flags.with_underline(match attrs.underline() {
        WezUnderline::None => Underline::None,
        WezUnderline::Single => Underline::Single,
        WezUnderline::Double => Underline::Double,
        WezUnderline::Curly => Underline::Curly,
        WezUnderline::Dotted => Underline::Dotted,
        WezUnderline::Dashed => Underline::Dashed,
    });
    CellStyle {
        fg: color(attrs.foreground()),
        bg: color(attrs.background()),
        underline_color: color(attrs.underline_color()),
        flags,
    }
}

/// A colour as the terminal states it, not as it would be drawn: an indexed
/// colour stays indexed so a palette change repaints without resending rows.
pub(crate) fn color(attr: ColorAttribute) -> WireColor {
    match attr {
        ColorAttribute::Default => WireColor::Default,
        ColorAttribute::PaletteIndex(i) => WireColor::Indexed(i),
        ColorAttribute::TrueColorWithPaletteFallback(rgb, _)
        | ColorAttribute::TrueColorWithDefaultFallback(rgb) => WireColor::Rgb(rgb8(rgb)),
    }
}

/// WezTerm keeps colours as linear-ish f32 sRGBA; the wire keeps 8 bits per
/// channel and no alpha, because a terminal cell's colour is opaque and the
/// renderer's own alpha is a client decision.
fn rgb8(color: SrgbaTuple) -> [u8; 3] {
    let (r, g, b, _) = color.as_rgba_u8();
    [r, g, b]
}

pub(crate) fn palette(palette: &ColorPalette) -> Palette {
    let mut ansi = [[0u8; 3]; 16];
    for (out, color) in ansi.iter_mut().zip(palette.colors.0.iter()) {
        *out = rgb8(*color);
    }
    Palette {
        ansi,
        foreground: rgb8(palette.foreground),
        background: rgb8(palette.background),
        cursor: rgb8(palette.cursor_bg),
    }
}

/// The modes a renderer or an input router has to know about. `mouse_grabbed`
/// covers all three tracking modes at once, which is exactly the question the
/// client asks: does my mouse belong to the child or to the selection?
pub(crate) fn modes(terminal: &Terminal) -> Modes {
    Modes {
        alt_screen: terminal.is_alt_screen_active(),
        mouse_reporting: terminal.is_mouse_grabbed(),
        bracketed_paste: terminal.bracketed_paste_enabled(),
        reverse_video: terminal.get_reverse_video(),
        focus_reporting: terminal.focus_tracking_enabled(),
    }
}

/// The cursor in visible-grid coordinates, clamped into the grid.
///
/// A resize can leave the terminal's cursor row outside the new screen for as
/// long as it takes the child to notice `SIGWINCH`; a client that indexes its
/// row array with `y` must not be handed that.
pub(crate) fn cursor(pos: CursorPosition, cols: usize, rows: usize) -> Cursor {
    let (shape, blinking) = match pos.shape {
        // `Default` is whatever the embedder draws when the child never said:
        // a blinking block, the same as an unconfigured xterm.
        WezCursorShape::Default | WezCursorShape::BlinkingBlock => (CursorShape::Block, true),
        WezCursorShape::SteadyBlock => (CursorShape::Block, false),
        WezCursorShape::BlinkingUnderline => (CursorShape::Underline, true),
        WezCursorShape::SteadyUnderline => (CursorShape::Underline, false),
        WezCursorShape::BlinkingBar => (CursorShape::Bar, true),
        WezCursorShape::SteadyBar => (CursorShape::Bar, false),
    };
    let x = pos.x.min(cols.saturating_sub(1)).min(u16::MAX as usize) as u16;
    let y = pos
        .y
        .clamp(0, rows.saturating_sub(1).min(u16::MAX as usize) as i64) as u16;
    Cursor {
        x,
        y,
        shape,
        visible: pos.visibility == CursorVisibility::Visible,
        blinking,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client decides which palette entries a theme may fill by comparing
    /// them against `Palette::RUNNER_DEFAULT`. That comparison is only
    /// meaningful while the constant is exactly what this runner reports for
    /// a terminal no `OSC 4`/`10`/`11` has touched.
    #[test]
    fn an_untouched_palette_is_the_constant_clients_compare_against() {
        assert_eq!(palette(&ColorPalette::default()), Palette::RUNNER_DEFAULT);
    }
}
