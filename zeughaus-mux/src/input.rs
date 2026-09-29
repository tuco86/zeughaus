//! What a controlling client sends a terminal: semantic keys, text, paste,
//! mouse, size and viewport.
//!
//! The runner turns a [`KeyInput`] into bytes where the terminal's keyboard
//! modes are known (application cursor keys, `modifyOtherKeys`, bracketed
//! paste). A client never encodes an escape sequence.
//!
//! Every input that reaches the child carries a `serial` from a per-client
//! counter; the runner echoes the highest applied one in every head and delta
//! (`input_serial_ack`), which is how a client measures round-trip time and
//! refuses a cursor position older than its own last keystroke. Serials are
//! never replayed: after a redial the client starts a new exchange, and
//! whatever was in flight is gone rather than typed twice.

use serde::{Deserialize, Serialize};

use crate::terminal::Dimensions;

/// Modifier keys held, as flag bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const SHIFT: u8 = 1 << 0;
    pub const CTRL: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    pub const SUPER: u8 = 1 << 3;

    pub fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }

    pub fn with(self, flag: u8) -> Self {
        Modifiers(self.0 | flag)
    }
}

/// A key without a text meaning of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamedKey {
    Enter,
    Tab,
    Backspace,
    Escape,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    /// `F1` is `F(1)`.
    F(u8),
}

/// One keystroke as the client saw it.
///
/// `Char` carries the character the key produces with the modifiers already
/// applied by the platform (`Shift+a` is `'A'`), except that `Ctrl` and `Alt`
/// are left to the runner: `Ctrl+c` arrives as `Char('c')` with
/// [`Modifiers::CTRL`], because only the terminal knows whether that is
/// `0x03` or a `modifyOtherKeys` sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    Char(char),
    Named(NamedKey),
}

/// Whether a key went down or came back up. A held key's auto-repeat is
/// another `Press`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyKind {
    Press,
    /// Only a child that asked for event types (kitty's keyboard protocol)
    /// hears it; the runner drops it for everything else.
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyInput {
    pub key: Key,
    pub modifiers: Modifiers,
    pub kind: KeyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Wheel up one notch.
    WheelUp,
    /// Wheel down one notch.
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseKind {
    Press,
    Release,
    /// Movement with `button` held, or `None` for plain motion.
    Move,
}

/// A mouse event in grid coordinates of the visible screen. Sent while the
/// terminal reports the mouse ([`crate::Modes::mouse_reporting`]), and for
/// the wheel also on the alternate screen, where the runner turns it into
/// cursor keys for a child that does not read the mouse; otherwise the mouse
/// selects text locally and nothing travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MouseInput {
    pub kind: MouseKind,
    pub button: Option<MouseButton>,
    pub col: u16,
    pub row: u16,
    pub modifiers: Modifiers,
}

/// One command on a terminal exchange, client to runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalCommand {
    Key {
        serial: u64,
        input: KeyInput,
    },
    /// Committed text: an IME result or a character the key model does not
    /// carry. Written verbatim.
    Text {
        serial: u64,
        text: String,
    },
    /// Pasted text; wrapped in bracketed-paste markers when the terminal
    /// asked for them, and with line endings normalized by the runner.
    Paste {
        serial: u64,
        text: String,
    },
    Mouse {
        serial: u64,
        input: MouseInput,
    },
    /// The controller's view size in cells. Ignored from a viewer.
    Resize(Dimensions),
    /// The rows this client currently shows, for the runner to send
    /// replacements for (a viewer scrolled into scrollback watches those
    /// rows, not the screen).
    Viewport {
        first_row: i64,
        rows: u16,
    },
    Focus(bool),
}

/// Longest text or paste accepted in one command. A paste beyond it is
/// refused by the client before it is sent.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

impl TerminalCommand {
    /// The input serial of a command that carries one.
    pub fn serial(&self) -> Option<u64> {
        match self {
            TerminalCommand::Key { serial, .. }
            | TerminalCommand::Text { serial, .. }
            | TerminalCommand::Paste { serial, .. }
            | TerminalCommand::Mouse { serial, .. } => Some(*serial),
            _ => None,
        }
    }
}
