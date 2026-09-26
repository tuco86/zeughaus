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
//!
//! macOS has no AltGr: Option both composes characters and is the only Alt.
//! There the left Option is Meta, like the left Alt elsewhere, and the right
//! Option composes, like AltGr: right `Option+L` on a German layout types `@`.

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

/// The platform whose keyboard conventions apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Other,
}

impl Platform {
    pub const CURRENT: Platform = if cfg!(target_os = "macos") {
        Platform::Mac
    } else {
        Platform::Other
    };
}

/// Which Alt/Option keys are down, tracked from their own press and release events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AltSide {
    pub left: bool,
    pub right: bool,
}

/// The shortcuts that are the terminal widget's, not the child's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chord {
    Copy,
    Paste,
    TakeControl,
    Release,
}

/// The widget shortcut `key` is, if any: `Ctrl+Shift+C/V/T/Escape` on every
/// platform, and on macOS also `Cmd+C/V/T/Escape` (with or without Shift),
/// since Command belongs to the application there and never to the child.
pub(crate) fn chord(key: &Key, from: IcedModifiers, platform: Platform) -> Option<Chord> {
    let held = (from.control() && from.shift())
        || (platform == Platform::Mac && from.logo() && !from.control());
    if !held {
        return None;
    }
    match key {
        Key::Named(Named::Escape) => Some(Chord::Release),
        Key::Character(text) => match text.chars().next()?.to_ascii_lowercase() {
            'c' => Some(Chord::Copy),
            'v' => Some(Chord::Paste),
            't' => Some(Chord::TakeControl),
            _ => None,
        },
        _ => None,
    }
}

/// One keystroke, or `None` for a key the terminal has no meaning for
/// (a bare modifier, a media key, a multi-scalar composition result -- the
/// last of those reaches the child as committed text instead).
///
/// `key` should be iced's `modified_key`: the layout's result for the physical
/// key with Shift and AltGr applied. `bare` is iced's `key`, the same key
/// without modifiers, and `alt` which Alt/Option keys are down.
///
/// On macOS a character typed with Option is Meta plus the bare character
/// unless only the right Option is down, which composes and sends the
/// layout's result without Alt. Meta+Shift+letter sends the uppercase letter;
/// Meta+Shift+symbol sends the unshifted symbol with Shift and Alt, because
/// iced reports no character with Shift applied but Option not.
pub fn key_input(
    key: &Key,
    bare: &Key,
    from: IcedModifiers,
    alt: AltSide,
    platform: Platform,
) -> Option<KeyInput> {
    if platform == Platform::Mac
        && from.alt()
        && let Key::Character(text) = key
    {
        if alt.right && !alt.left {
            return Some(KeyInput {
                key: MuxKey::Char(single_char(text)?),
                modifiers: modifiers(from.difference(IcedModifiers::ALT)),
            });
        }
        if let Key::Character(bare_text) = bare {
            let mut c = single_char(bare_text)?;
            if from.shift() {
                let mut upper = c.to_uppercase();
                if let (Some(u), None) = (upper.next(), upper.next()) {
                    c = u;
                }
            }
            return Some(KeyInput {
                key: MuxKey::Char(c),
                modifiers: modifiers(from),
            });
        }
    }
    let modifiers = modifiers(from);
    let key = match key {
        Key::Character(text) => MuxKey::Char(single_char(text)?),
        Key::Named(Named::Space) => MuxKey::Char(' '),
        Key::Named(named) => MuxKey::Named(named_key(*named)?),
        Key::Unidentified => return None,
    };
    Some(KeyInput { key, modifiers })
}

/// The one scalar of `text`, or `None` for an empty or multi-scalar text.
fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
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
        key_input(&key, &key, from, AltSide::default(), Platform::Other)
    }

    fn mac(key: &str, bare: &str, from: IcedModifiers, alt: AltSide) -> Option<KeyInput> {
        key_input(
            &Key::Character(key.into()),
            &Key::Character(bare.into()),
            from,
            alt,
            Platform::Mac,
        )
    }

    const LEFT: AltSide = AltSide {
        left: true,
        right: false,
    };
    const RIGHT: AltSide = AltSide {
        left: false,
        right: true,
    };

    #[test]
    fn right_option_composes_without_alt() {
        assert_eq!(
            mac("@", "l", IcedModifiers::ALT, RIGHT),
            Some(KeyInput {
                key: MuxKey::Char('@'),
                modifiers: Modifiers::default(),
            })
        );
    }

    #[test]
    fn left_option_is_meta_with_the_bare_character() {
        assert_eq!(
            mac("@", "l", IcedModifiers::ALT, LEFT),
            Some(KeyInput {
                key: MuxKey::Char('l'),
                modifiers: Modifiers::default().with(Modifiers::ALT),
            })
        );
        let input = mac("ı", "b", IcedModifiers::ALT | IcedModifiers::SHIFT, LEFT)
            .expect("meta+shift+b is a keystroke");
        assert_eq!(input.key, MuxKey::Char('B'));
        assert!(input.modifiers.has(Modifiers::ALT));
    }

    #[test]
    fn named_keys_keep_alt_on_either_option() {
        let arrow = Key::Named(Named::ArrowLeft);
        let input = key_input(&arrow, &arrow, IcedModifiers::ALT, RIGHT, Platform::Mac)
            .expect("option+left is a keystroke");
        assert_eq!(input.key, MuxKey::Named(NamedKey::Left));
        assert!(input.modifiers.has(Modifiers::ALT));
    }

    #[test]
    fn alt_elsewhere_sends_the_layouts_character_with_alt() {
        assert_eq!(
            press(Key::Character("b".into()), IcedModifiers::ALT),
            Some(KeyInput {
                key: MuxKey::Char('b'),
                modifiers: Modifiers::default().with(Modifiers::ALT),
            })
        );
    }

    #[test]
    fn chords_are_ctrl_shift_everywhere_and_cmd_on_mac() {
        let ctrl_shift = IcedModifiers::CTRL | IcedModifiers::SHIFT;
        let c = Key::Character("c".into());
        for platform in [Platform::Mac, Platform::Other] {
            assert_eq!(
                chord(&Key::Character("C".into()), ctrl_shift, platform),
                Some(Chord::Copy)
            );
        }
        assert_eq!(
            chord(&c, IcedModifiers::LOGO, Platform::Mac),
            Some(Chord::Copy)
        );
        assert_eq!(chord(&c, IcedModifiers::LOGO, Platform::Other), None);
        assert_eq!(
            chord(
                &Key::Named(Named::Escape),
                IcedModifiers::LOGO,
                Platform::Mac
            ),
            Some(Chord::Release)
        );
        assert_eq!(chord(&c, IcedModifiers::CTRL, Platform::Other), None);
        assert_eq!(chord(&c, IcedModifiers::CTRL, Platform::Mac), None);
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
