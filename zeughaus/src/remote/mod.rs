//! The editor without a window, driven over a Unix socket.
//!
//! `zeughaus --headless --control <socket> [--size <W>x<H>] [--scale <f>]`
//! runs the real [`App`](crate::app::App) offscreen with a
//! hardware wgpu renderer; `zeughaus ctl <socket> <words...>` sends it one
//! command line and prints the one-line reply, `ok ...` or `err <reason>`.
//! Coordinates are logical pixels.

mod ctl;
mod host;

use std::path::PathBuf;
use std::time::Duration;

use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers};
use iced::mouse;
use iced::{Point, Size};

/// Runs the control client or the headless host when the command line asks
/// for one, and returns the process exit code. `None` is every other command
/// line: the windowed editor starts as usual.
pub fn dispatch() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "ctl") {
        return Some(ctl::run(&args[1..]));
    }
    if !args.iter().any(|a| a == "--headless") {
        return None;
    }
    Some(match HostArgs::parse(&args) {
        Ok(host) => host::run(host),
        Err(e) => {
            eprintln!("[remote] {e}");
            eprintln!(
                "usage: zeughaus --headless --control <socket> [--size <W>x<H>] [--scale <f>]"
            );
            2
        }
    })
}

/// What `--headless` was started with.
struct HostArgs {
    control: PathBuf,
    /// Logical size of the pretend window.
    size: Size,
    scale: f32,
}

impl HostArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut control = None;
        let mut size = crate::WINDOW_SIZE;
        let mut scale = 1.0;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--headless" => {}
                "--control" => control = Some(PathBuf::from(value()?)),
                "--size" => size = parse_size(value()?)?,
                "--scale" => scale = parse_scale(value()?)?,
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        Ok(Self {
            control: control.ok_or("--control <socket> is required")?,
            size,
            scale,
        })
    }
}

fn parse_size(text: &str) -> Result<Size, String> {
    let (w, h) = text
        .split_once('x')
        .ok_or_else(|| format!("size {text:?} is not <W>x<H>"))?;
    let w = parse_extent(w)?;
    let h = parse_extent(h)?;
    Ok(Size::new(w, h))
}

/// One side of the window: positive and finite, or the layout has nothing to
/// divide.
fn parse_extent(text: &str) -> Result<f32, String> {
    match text.parse::<f32>() {
        Ok(v) if v.is_finite() && v >= 1.0 => Ok(v),
        _ => Err(format!("{text:?} is not a size of at least 1")),
    }
}

fn parse_scale(text: &str) -> Result<f32, String> {
    match text.parse::<f32>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(v),
        _ => Err(format!("scale {text:?} is not a positive number")),
    }
}

/// One line of the control protocol.
#[derive(Debug, Clone, PartialEq)]
enum Command {
    Size,
    /// `over` spreads the move from the current cursor across that time.
    Move {
        to: Point,
        over: Duration,
    },
    Down(mouse::Button),
    Up(mouse::Button),
    /// Modifiers held across the click, like a Ctrl+click on a link.
    Click(Point, mouse::Button, Modifiers),
    DoubleClick(Point),
    /// `over` spreads the path across that time; zero delivers it as fast
    /// as the interface settles after each step.
    Drag {
        from: Point,
        to: Point,
        steps: u32,
        over: Duration,
    },
    Scroll {
        at: Point,
        dx: f32,
        dy: f32,
    },
    Key(Keystroke),
    Type(String),
    Find(String),
    Screenshot(PathBuf),
    Record(PathBuf),
    RecordStop,
    Resize(Size),
    Scale(f32),
    Clip,
    ClipSet(String),
    WaitIdle(Duration),
    Restart,
    Quit,
}

/// A key press as a platform reports it: `key` without the modifiers,
/// `modified_key` with Shift applied, and the text it types, if any.
#[derive(Debug, Clone, PartialEq)]
struct Keystroke {
    key: Key,
    modified_key: Key,
    modifiers: Modifiers,
    text: Option<char>,
}

impl Command {
    fn parse(line: &str) -> Result<Self, String> {
        let line = line.trim_end_matches(['\r', '\n']);
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        let words: Vec<&str> = rest.split_whitespace().collect();
        let arity = |min: usize, max: usize| {
            if (min..=max).contains(&words.len()) {
                Ok(())
            } else {
                Err(format!("{name}: wrong number of arguments"))
            }
        };
        let command = match name {
            "size" => {
                arity(0, 0)?;
                Command::Size
            }
            "move" => {
                arity(2, 3)?;
                Command::Move {
                    to: point(words[0], words[1])?,
                    over: millis(words.get(2).copied())?,
                }
            }
            "down" | "up" => {
                arity(0, 1)?;
                let button = button(words.first().copied())?;
                if name == "down" {
                    Command::Down(button)
                } else {
                    Command::Up(button)
                }
            }
            "click" => {
                arity(2, 4)?;
                Command::Click(
                    point(words[0], words[1])?,
                    button(words.get(2).copied())?,
                    held(words.get(3).copied())?,
                )
            }
            "dblclick" => {
                arity(2, 2)?;
                Command::DoubleClick(point(words[0], words[1])?)
            }
            "drag" => {
                arity(4, 6)?;
                let steps = match words.get(4) {
                    Some(s) => s
                        .parse::<u32>()
                        .ok()
                        .filter(|&s| s > 0)
                        .ok_or_else(|| format!("steps {s:?} is not a positive integer"))?,
                    None => 10,
                };
                Command::Drag {
                    from: point(words[0], words[1])?,
                    to: point(words[2], words[3])?,
                    steps,
                    over: millis(words.get(5).copied())?,
                }
            }
            "scroll" => {
                arity(3, 4)?;
                Command::Scroll {
                    at: point(words[0], words[1])?,
                    dy: number(words[2])?,
                    dx: words.get(3).map_or(Ok(0.0), |w| number(w))?,
                }
            }
            "key" => {
                arity(1, 1)?;
                Command::Key(Keystroke::parse(words[0])?)
            }
            "type" if !rest.is_empty() => Command::Type(rest.to_owned()),
            "find" if !rest.is_empty() => Command::Find(rest.to_owned()),
            "screenshot" if !rest.is_empty() => Command::Screenshot(PathBuf::from(rest)),
            "record" if !rest.is_empty() => Command::Record(PathBuf::from(rest)),
            "type" | "find" | "screenshot" | "record" => {
                return Err(format!("{name}: missing argument"));
            }
            "record-stop" => {
                arity(0, 0)?;
                Command::RecordStop
            }
            "resize" => {
                arity(2, 2)?;
                Command::Resize(Size::new(parse_extent(words[0])?, parse_extent(words[1])?))
            }
            "scale" => {
                arity(1, 1)?;
                Command::Scale(parse_scale(words[0])?)
            }
            "clip" => {
                arity(0, 0)?;
                Command::Clip
            }
            "clip-set" => Command::ClipSet(rest.to_owned()),
            "wait-idle" => {
                arity(0, 1)?;
                Command::WaitIdle(match words.first() {
                    Some(&ms) => millis(Some(ms))?,
                    None => Duration::from_millis(200),
                })
            }
            "restart" => {
                arity(0, 0)?;
                Command::Restart
            }
            "quit" => {
                arity(0, 0)?;
                Command::Quit
            }
            "" => return Err("empty command".to_owned()),
            other => return Err(format!("unknown command {other:?}")),
        };
        Ok(command)
    }
}

fn number(text: &str) -> Result<f32, String> {
    text.parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("{text:?} is not a number"))
}

/// An optional duration in milliseconds; missing is zero.
fn millis(text: Option<&str>) -> Result<Duration, String> {
    match text {
        None => Ok(Duration::ZERO),
        Some(ms) => ms
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| format!("{ms:?} is not a number of milliseconds")),
    }
}

fn point(x: &str, y: &str) -> Result<Point, String> {
    Ok(Point::new(number(x)?, number(y)?))
}

fn button(name: Option<&str>) -> Result<mouse::Button, String> {
    match name {
        None | Some("left") => Ok(mouse::Button::Left),
        Some("right") => Ok(mouse::Button::Right),
        Some("middle") => Ok(mouse::Button::Middle),
        Some(other) => Err(format!("unknown button {other:?}")),
    }
}

/// `ctrl`, `shift+alt`, ...: the modifiers a click is made with.
fn held(spec: Option<&str>) -> Result<Modifiers, String> {
    let mut modifiers = Modifiers::empty();
    for name in spec.into_iter().flat_map(|spec| spec.split('+')) {
        modifiers |= match name.to_ascii_lowercase().as_str() {
            "ctrl" => Modifiers::CTRL,
            "shift" => Modifiers::SHIFT,
            "alt" => Modifiers::ALT,
            "super" => Modifiers::LOGO,
            other => return Err(format!("unknown modifier {other:?}")),
        };
    }
    Ok(modifiers)
}

impl Keystroke {
    /// `[ctrl+][shift+][alt+][super+]KEY`, modifiers in any order. `KEY` is
    /// one character or a key name.
    fn parse(spec: &str) -> Result<Self, String> {
        let mut modifiers = Modifiers::empty();
        let mut rest = spec;
        'prefixes: loop {
            for (prefix, flag) in [
                ("ctrl+", Modifiers::CTRL),
                ("shift+", Modifiers::SHIFT),
                ("alt+", Modifiers::ALT),
                ("super+", Modifiers::LOGO),
            ] {
                if rest.len() > prefix.len()
                    && rest.is_char_boundary(prefix.len())
                    && rest[..prefix.len()].eq_ignore_ascii_case(prefix)
                {
                    modifiers |= flag;
                    rest = &rest[prefix.len()..];
                    continue 'prefixes;
                }
            }
            break;
        }
        let mut chars = rest.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Ok(Self::character(c, modifiers)),
            _ => {
                let named = named_key(rest).ok_or_else(|| format!("unknown key {rest:?}"))?;
                Ok(Self::named(named, modifiers))
            }
        }
    }

    /// A character key. Shift is applied the way a US layout applies it to
    /// letters; other characters are taken as given.
    fn character(c: char, modifiers: Modifiers) -> Self {
        if c == ' ' {
            return Self::named(Named::Space, modifiers);
        }
        let (bare, shifted) = if modifiers.shift() {
            (c.to_ascii_lowercase(), c.to_ascii_uppercase())
        } else {
            (c, c)
        };
        Self {
            key: Key::Character(bare.to_string().into()),
            modified_key: Key::Character(shifted.to_string().into()),
            modifiers,
            text: types_text(modifiers).then_some(shifted),
        }
    }

    fn named(named: Named, modifiers: Modifiers) -> Self {
        Self {
            key: Key::Named(named),
            modified_key: Key::Named(named),
            modifiers,
            // Space is the one named key that types something.
            text: (named == Named::Space && types_text(modifiers)).then_some(' '),
        }
    }
}

/// Ctrl, Alt and Super turn a key into a shortcut rather than text.
fn types_text(modifiers: Modifiers) -> bool {
    !(modifiers.control() || modifiers.alt() || modifiers.logo())
}

fn named_key(name: &str) -> Option<Named> {
    const KEYS: [(&str, Named); 26] = [
        ("enter", Named::Enter),
        ("escape", Named::Escape),
        ("tab", Named::Tab),
        ("backspace", Named::Backspace),
        ("delete", Named::Delete),
        ("space", Named::Space),
        ("arrowup", Named::ArrowUp),
        ("arrowdown", Named::ArrowDown),
        ("arrowleft", Named::ArrowLeft),
        ("arrowright", Named::ArrowRight),
        ("home", Named::Home),
        ("end", Named::End),
        ("pageup", Named::PageUp),
        ("pagedown", Named::PageDown),
        ("f1", Named::F1),
        ("f2", Named::F2),
        ("f3", Named::F3),
        ("f4", Named::F4),
        ("f5", Named::F5),
        ("f6", Named::F6),
        ("f7", Named::F7),
        ("f8", Named::F8),
        ("f9", Named::F9),
        ("f10", Named::F10),
        ("f11", Named::F11),
        ("f12", Named::F12),
    ];
    KEYS.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, key)| *key)
}
