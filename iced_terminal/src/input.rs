//! Turning iced's keyboard and mouse events into the mux's semantic input.
//!
//! The widget never encodes an escape sequence. Only the runner knows whether
//! the terminal is in application-cursor mode, what `modifyOtherKeys` is set
//! to, or whether the child asked for bracketed paste, so what travels is
//! "the user pressed Home with Shift", not `ESC [ 1 ; 2 H`.
//!
//! Characters arrive with the platform's modifiers already applied -- `Shift+a`
//! is `'A'`, an AltGr layout produces whatever it produces -- except `Ctrl`
//! and `Alt`, which stay as flags because `Ctrl+c` is `0x03` to one terminal
//! and a `modifyOtherKeys` sequence to another.

use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers as IcedModifiers};
use iced::mouse;
use zeughaus_mux::input::Key as MuxKey;
use zeughaus_mux::{KeyInput, Modifiers, MouseButton, NamedKey};

/// The modifier flags, as the wire states them.
pub fn modifiers(from: IcedModifiers) -> Modifiers {
    let mut out = Modifiers::default();
    if from.shift() {
        out = out.with(Modifiers::SHIFT);
    }
    if from.control() {
        out = out.with(Modifiers::CTRL);
    }
    if from.alt() {
        out = out.with(Modifiers::ALT);
    }
    if from.logo() {
        out = out.with(Modifiers::SUPER);
    }
    out
}

/// One keystroke, or `None` for a key the terminal has no meaning for
/// (a bare modifier, a media key, a multi-scalar composition result -- the
/// last of those reaches the child as committed text instead).
///
/// `key` should be iced's `modified_key`: the layout's result for the physical
/// key with Shift and AltGr applied.
pub fn key_input(key: &Key, from: IcedModifiers) -> Option<KeyInput> {
    let modifiers = modifiers(from);
    let key = match key {
        Key::Character(text) => {
            let mut chars = text.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            MuxKey::Char(first)
        }
        Key::Named(Named::Space) => MuxKey::Char(' '),
        Key::Named(named) => MuxKey::Named(named_key(*named)?),
        Key::Unidentified => return None,
    };
    Some(KeyInput { key, modifiers })
}

fn named_key(named: Named) -> Option<NamedKey> {
    Some(match named {
        Named::Enter => NamedKey::Enter,
        Named::Tab => NamedKey::Tab,
        Named::Backspace => NamedKey::Backspace,
        Named::Escape => NamedKey::Escape,
        Named::Insert => NamedKey::Insert,
        Named::Delete => NamedKey::Delete,
        Named::Home => NamedKey::Home,
        Named::End => NamedKey::End,
        Named::PageUp => NamedKey::PageUp,
        Named::PageDown => NamedKey::PageDown,
        Named::ArrowUp => NamedKey::Up,
        Named::ArrowDown => NamedKey::Down,
        Named::ArrowLeft => NamedKey::Left,
        Named::ArrowRight => NamedKey::Right,
        Named::F1 => NamedKey::F(1),
        Named::F2 => NamedKey::F(2),
        Named::F3 => NamedKey::F(3),
        Named::F4 => NamedKey::F(4),
        Named::F5 => NamedKey::F(5),
        Named::F6 => NamedKey::F(6),
        Named::F7 => NamedKey::F(7),
        Named::F8 => NamedKey::F(8),
        Named::F9 => NamedKey::F(9),
        Named::F10 => NamedKey::F(10),
        Named::F11 => NamedKey::F(11),
        Named::F12 => NamedKey::F(12),
        Named::F13 => NamedKey::F(13),
        Named::F14 => NamedKey::F(14),
        Named::F15 => NamedKey::F(15),
        Named::F16 => NamedKey::F(16),
        Named::F17 => NamedKey::F(17),
        Named::F18 => NamedKey::F(18),
        Named::F19 => NamedKey::F(19),
        Named::F20 => NamedKey::F(20),
        Named::F21 => NamedKey::F(21),
        Named::F22 => NamedKey::F(22),
        Named::F23 => NamedKey::F(23),
        Named::F24 => NamedKey::F(24),
        _ => return None,
    })
}

/// The pointer buttons a terminal reports. Back/forward have no mapping and
/// are dropped rather than invented.
pub fn mouse_button(button: mouse::Button) -> Option<MouseButton> {
    Some(match button {
        mouse::Button::Left => MouseButton::Left,
        mouse::Button::Middle => MouseButton::Middle,
        mouse::Button::Right => MouseButton::Right,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key, from: IcedModifiers) -> Option<KeyInput> {
        key_input(&key, from)
    }

    #[test]
    fn named_keys_map_to_their_wire_names() {
        assert_eq!(
            press(Key::Named(Named::Enter), IcedModifiers::empty()),
            Some(KeyInput {
                key: MuxKey::Named(NamedKey::Enter),
                modifiers: Modifiers::default(),
            })
        );
        for (named, expected) in [
            (Named::ArrowUp, NamedKey::Up),
            (Named::ArrowDown, NamedKey::Down),
            (Named::ArrowLeft, NamedKey::Left),
            (Named::ArrowRight, NamedKey::Right),
        ] {
            assert_eq!(
                press(Key::Named(named), IcedModifiers::empty()),
                Some(KeyInput {
                    key: MuxKey::Named(expected),
                    modifiers: Modifiers::default(),
                })
            );
        }
        assert_eq!(
            press(Key::Named(Named::F5), IcedModifiers::empty()),
            Some(KeyInput {
                key: MuxKey::Named(NamedKey::F(5)),
                modifiers: Modifiers::default(),
            })
        );
    }

    #[test]
    fn control_stays_a_modifier_so_the_runner_can_encode_it() {
        let input =
            press(Key::Character("c".into()), IcedModifiers::CTRL).expect("ctrl+c is a keystroke");
        assert_eq!(input.key, MuxKey::Char('c'));
        assert!(input.modifiers.has(Modifiers::CTRL));
        assert!(!input.modifiers.has(Modifiers::SHIFT));
    }

    #[test]
    fn shift_is_already_applied_to_the_character() {
        // iced's `modified_key` carries the layout's result.
        let input = press(Key::Character("A".into()), IcedModifiers::SHIFT)
            .expect("shift+a is a keystroke");
        assert_eq!(input.key, MuxKey::Char('A'));
        assert!(input.modifiers.has(Modifiers::SHIFT));
    }

    #[test]
    fn space_is_a_character_not_a_named_key() {
        let input =
            press(Key::Named(Named::Space), IcedModifiers::empty()).expect("space is a keystroke");
        assert_eq!(input.key, MuxKey::Char(' '));
    }

    #[test]
    fn keys_without_a_terminal_meaning_are_dropped() {
        assert_eq!(
            press(Key::Named(Named::Shift), IcedModifiers::empty()),
            None
        );
        assert_eq!(press(Key::Unidentified, IcedModifiers::empty()), None);
        // A composition that produced several scalars is committed text.
        assert_eq!(
            press(Key::Character("ab".into()), IcedModifiers::empty()),
            None
        );
    }

    #[test]
    fn only_the_buttons_a_terminal_reports_are_forwarded() {
        assert_eq!(mouse_button(mouse::Button::Left), Some(MouseButton::Left));
        assert_eq!(
            mouse_button(mouse::Button::Middle),
            Some(MouseButton::Middle)
        );
        assert_eq!(mouse_button(mouse::Button::Right), Some(MouseButton::Right));
        assert_eq!(mouse_button(mouse::Button::Back), None);
    }
}
