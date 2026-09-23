//! Reading a WezTerm colour TOML.
//!
//! The format every terminal scheme in the wild is published in, so it is the
//! one the bundled pack and a file in the state directory both speak. Only the
//! `[colors]` table is read; `[metadata]`, `[colors.indexed]` and anything
//! else a scheme carries is none of our business.

use iced::Color;
use serde::Deserialize;

use crate::Scheme;

/// Why a colour TOML is not a scheme. The message names the key at fault, so
/// the editor can put it on stderr and the user can fix the file.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("missing `{0}`")]
    Missing(&'static str),
    #[error("`{key}` needs 8 colours, found {found}")]
    Length { key: &'static str, found: usize },
    #[error("`{key}` is not an `#rrggbb` colour: `{value}`")]
    Color { key: String, value: String },
}

#[derive(Deserialize)]
struct File {
    colors: Option<Colors>,
}

#[derive(Deserialize)]
struct Colors {
    ansi: Option<Vec<String>>,
    brights: Option<Vec<String>>,
    foreground: Option<String>,
    background: Option<String>,
    cursor_bg: Option<String>,
    selection_bg: Option<String>,
}

pub fn parse(source: &str) -> Result<Scheme, ParseError> {
    let colors = toml::from_str::<File>(source)?
        .colors
        .ok_or(ParseError::Missing("colors"))?;

    let mut ansi = [Color::TRANSPARENT; 16];
    ansi[..8].copy_from_slice(&eight(colors.ansi, "colors.ansi")?);
    ansi[8..].copy_from_slice(&eight(colors.brights, "colors.brights")?);

    let foreground = required(colors.foreground, "colors.foreground")?;
    let background = required(colors.background, "colors.background")?;

    Ok(Scheme {
        ansi,
        foreground,
        background,
        // A scheme that names no cursor draws it in the foreground, and one
        // that names no selection gets a tint of foreground over the
        // background -- visible in either tone, unlike a fixed grey.
        cursor: optional(colors.cursor_bg, "colors.cursor_bg")?.unwrap_or(foreground),
        selection: optional(colors.selection_bg, "colors.selection_bg")?
            .unwrap_or_else(|| iced::theme::palette::mix(background, foreground, 0.3)),
    })
}

fn eight(list: Option<Vec<String>>, key: &'static str) -> Result<[Color; 8], ParseError> {
    let list = list.ok_or(ParseError::Missing(key))?;

    if list.len() != 8 {
        return Err(ParseError::Length {
            key,
            found: list.len(),
        });
    }

    let mut colors = [Color::TRANSPARENT; 8];
    for (index, (slot, value)) in colors.iter_mut().zip(&list).enumerate() {
        *slot = hex(&format!("{key}[{index}]"), value)?;
    }

    Ok(colors)
}

fn required(value: Option<String>, key: &'static str) -> Result<Color, ParseError> {
    hex(key, &value.ok_or(ParseError::Missing(key))?)
}

fn optional(value: Option<String>, key: &'static str) -> Result<Option<Color>, ParseError> {
    value.map(|value| hex(key, &value)).transpose()
}

/// `#rrggbb`, with the `#` optional: files written by hand drop it often
/// enough that refusing one would only be pedantry.
fn hex(key: &str, value: &str) -> Result<Color, ParseError> {
    let digits = value.strip_prefix('#').unwrap_or(value);

    let invalid = || ParseError::Color {
        key: key.to_owned(),
        value: value.to_owned(),
    };

    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }

    let packed = u32::from_str_radix(digits, 16).map_err(|_| invalid())?;

    Ok(Color::from_rgb8(
        (packed >> 16) as u8,
        (packed >> 8) as u8,
        packed as u8,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r##"
[metadata]
name = "test"

[colors]
foreground = "#f8f8f2"
background = "#282a36"
cursor_bg = "#bd93f9"
selection_bg = "#44475a"
ansi = ["#000000","#110000","#220000","#330000","#440000","#550000","#660000","#770000"]
brights = ["#880000","#990000","#aa0000","#bb0000","#cc0000","#dd0000","#ee0000","#ff0000"]

[colors.indexed]
"##;

    #[test]
    fn reads_a_full_scheme() {
        let scheme = parse(FULL).unwrap();

        assert_eq!(scheme.ansi[0], Color::from_rgb8(0x00, 0x00, 0x00));
        assert_eq!(scheme.ansi[7], Color::from_rgb8(0x77, 0x00, 0x00));
        assert_eq!(scheme.ansi[8], Color::from_rgb8(0x88, 0x00, 0x00));
        assert_eq!(scheme.ansi[15], Color::from_rgb8(0xff, 0x00, 0x00));
        assert_eq!(scheme.foreground, Color::from_rgb8(0xf8, 0xf8, 0xf2));
        assert_eq!(scheme.background, Color::from_rgb8(0x28, 0x2a, 0x36));
        assert_eq!(scheme.cursor, Color::from_rgb8(0xbd, 0x93, 0xf9));
        assert_eq!(scheme.selection, Color::from_rgb8(0x44, 0x47, 0x5a));
    }

    #[test]
    fn cursor_and_selection_fall_back() {
        let source = FULL
            .replace("cursor_bg = \"#bd93f9\"\n", "")
            .replace("selection_bg = \"#44475a\"\n", "");
        let scheme = parse(&source).unwrap();

        assert_eq!(scheme.cursor, scheme.foreground);
        assert_ne!(scheme.selection, scheme.background);
        assert_ne!(scheme.selection, scheme.foreground);
    }

    #[test]
    fn a_missing_key_names_itself() {
        let source = FULL.replace("background = \"#282a36\"\n", "");
        let error = parse(&source).unwrap_err().to_string();
        assert!(error.contains("colors.background"), "{error}");

        let source = FULL.replace("brights", "brighter");
        let error = parse(&source).unwrap_err().to_string();
        assert!(error.contains("colors.brights"), "{error}");

        let error = parse("[metadata]\nname = \"x\"\n").unwrap_err().to_string();
        assert!(error.contains("colors"), "{error}");
    }

    #[test]
    fn a_short_list_names_itself() {
        let source = FULL.replace(",\"#770000\"", "");
        let error = parse(&source).unwrap_err().to_string();
        assert!(error.contains("colors.ansi"), "{error}");
        assert!(error.contains('7'), "{error}");
    }

    #[test]
    fn the_hash_is_optional_but_the_digits_are_not() {
        let source = FULL.replace("\"#f8f8f2\"", "\"f8f8f2\"");
        assert_eq!(
            parse(&source).unwrap().foreground,
            Color::from_rgb8(0xf8, 0xf8, 0xf2)
        );

        let source = FULL.replace("\"#f8f8f2\"", "\"#f8f8\"");
        let error = parse(&source).unwrap_err().to_string();
        assert!(error.contains("colors.foreground"), "{error}");

        let source = FULL.replace("\"#110000\"", "\"#gg0000\"");
        let error = parse(&source).unwrap_err().to_string();
        assert!(error.contains("colors.ansi[1]"), "{error}");
    }
}
