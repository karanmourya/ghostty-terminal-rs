//! Terminal configuration.
//!
//! Ghostty uses a flat `key = value` file, so this module parses the same shape
//! rather than inventing a new format:
//!
//! ```toml
//! font-family = "Cascadia Mono"
//! font-size = 16
//! theme = "catppuccin-mocha"
//! background = "#1e1e2e"
//! foreground = "#cdd6f4"
//! palette = 3=#f9e2af
//! padding = 8
//! cursor-style = "bar"
//! scrollback-limit = 5000
//! ```
//!
//! The file is read from `GHOSTTY_WIN_CONFIG` when set, otherwise from
//! `%APPDATA%\ghostty-windows\config.toml`. Unknown keys produce a warning but
//! never prevent the terminal from starting, matching Ghostty's forgiving
//! stance for forward compatibility.

use std::path::PathBuf;

use crate::term::{DEFAULT_BG, DEFAULT_FG, PALETTE, SCROLLBACK_LIMIT};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub font_family: Option<String>,
    pub font_size: f32,
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub palette: [[u8; 3]; 16],
    pub padding: f32,
    pub cursor_style: CursorStyle,
    pub scrollback: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: 15.0,
            background: DEFAULT_BG,
            foreground: DEFAULT_FG,
            palette: PALETTE,
            padding: 6.0,
            cursor_style: CursorStyle::Block,
            scrollback: SCROLLBACK_LIMIT,
        }
    }
}

/// Where the config file is expected when `GHOSTTY_WIN_CONFIG` is not set.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|dir| dir.join("ghostty-windows").join("config.toml"))
}

/// Load the user config. Returns the config, any warnings, and the path used.
pub fn load() -> (Config, Vec<String>, Option<PathBuf>) {
    let path = std::env::var_os("GHOSTTY_WIN_CONFIG")
        .map(PathBuf::from)
        .or_else(default_path);
    let Some(path) = path else {
        return (Config::default(), Vec::new(), None);
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (Config::default(), Vec::new(), Some(path));
    };
    let (config, warnings) = parse(&text);
    (config, warnings, Some(path))
}

/// Parse the flat `key = value` format.
pub fn parse(text: &str) -> (Config, Vec<String>) {
    let mut config = Config::default();
    let mut warnings = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("line {}: expected `key = value`", index + 1));
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        let number = |warnings: &mut Vec<String>| match value.parse::<f32>() {
            Ok(n) if n > 0.0 => Some(n),
            _ => {
                warnings.push(format!(
                    "line {}: `{key}` needs a positive number",
                    index + 1
                ));
                None
            }
        };

        match key {
            "font-family" => {
                if value.is_empty() {
                    config.font_family = None;
                } else {
                    config.font_family = Some(value.to_string());
                }
            }
            "font-size" => {
                if let Some(size) = number(&mut warnings) {
                    config.font_size = size;
                }
            }
            "padding" | "window-padding-x" => {
                if let Some(padding) = number(&mut warnings) {
                    config.padding = padding;
                }
            }
            "window-padding-y" => {
                // The renderer uses one padding value; accept and ignore Y so
                // shared Ghostty configs do not warn.
            }
            "background" => match parse_hex(value) {
                Some(color) => config.background = color,
                None => warnings.push(format!("line {}: bad colour `{value}`", index + 1)),
            },
            "foreground" => match parse_hex(value) {
                Some(color) => config.foreground = color,
                None => warnings.push(format!("line {}: bad colour `{value}`", index + 1)),
            },
            "palette" => match parse_palette(value) {
                Some((slot, color)) => config.palette[slot] = color,
                None => warnings.push(format!(
                    "line {}: expected `palette = N=#rrggbb`",
                    index + 1
                )),
            },
            "cursor-style" => match value {
                "block" => config.cursor_style = CursorStyle::Block,
                "bar" | "line" => config.cursor_style = CursorStyle::Bar,
                "underline" => config.cursor_style = CursorStyle::Underline,
                _ => warnings.push(format!(
                    "line {}: unknown cursor style `{value}`",
                    index + 1
                )),
            },
            "scrollback-limit" => match value.parse::<usize>() {
                Ok(0) | Err(_) => warnings.push(format!(
                    "line {}: `scrollback-limit` needs a positive integer",
                    index + 1
                )),
                Ok(limit) => config.scrollback = limit,
            },
            "theme" => match theme(value) {
                Some((fg, bg, palette)) => {
                    config.foreground = fg;
                    config.background = bg;
                    config.palette = palette;
                }
                None => warnings.push(format!("line {}: unknown theme `{value}`", index + 1)),
            },
            _ => warnings.push(format!("line {}: unknown key `{key}`", index + 1)),
        }
    }

    (config, warnings)
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// `#rgb`, `#rrggbb`, and the same without the leading `#`.
fn parse_hex(value: &str) -> Option<[u8; 3]> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    match hex.len() {
        3 => {
            let mut out = [0u8; 3];
            for (i, c) in hex.bytes().enumerate() {
                let n = nibble(c)?;
                out[i] = n * 17;
            }
            Some(out)
        }
        6 => {
            let mut out = [0u8; 3];
            for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
                out[i] = nibble(chunk[0])? * 16 + nibble(chunk[1])?;
            }
            Some(out)
        }
        _ => None,
    }
}

fn parse_palette(value: &str) -> Option<(usize, [u8; 3])> {
    let (slot, color) = value.split_once('=')?;
    let slot = slot.trim().parse::<usize>().ok()?;
    if slot >= 16 {
        return None;
    }
    Some((slot, parse_hex(color.trim())?))
}

/// (foreground, background, palette)
type Theme = ([u8; 3], [u8; 3], [[u8; 3]; 16]);

fn theme(name: &str) -> Option<Theme> {
    match name {
        "default" | "dark" => Some((DEFAULT_FG, DEFAULT_BG, PALETTE)),
        "light" => Some((
            [0x65, 0x7b, 0x83],
            [0xfd, 0xf6, 0xe3],
            [
                [0x07, 0x36, 0x42],
                [0xdc, 0x32, 0x2f],
                [0x85, 0x99, 0x00],
                [0xb5, 0x89, 0x00],
                [0x26, 0x8b, 0xd2],
                [0xd3, 0x36, 0x82],
                [0x2a, 0xa1, 0x98],
                [0xee, 0xe8, 0xd5],
                [0x00, 0x2b, 0x36],
                [0xcb, 0x4b, 0x16],
                [0x58, 0x6e, 0x75],
                [0x65, 0x7b, 0x83],
                [0x83, 0x94, 0x96],
                [0x6c, 0x71, 0xc4],
                [0x93, 0xa1, 0xa1],
                [0xfd, 0xf6, 0xe3],
            ],
        )),
        "catppuccin-mocha" | "mocha" => Some((
            [0xcd, 0xd6, 0xf4],
            [0x1e, 0x1e, 0x2e],
            [
                [0x45, 0x47, 0x5a],
                [0xf3, 0x8b, 0xa8],
                [0xa6, 0xe3, 0xa1],
                [0xf9, 0xe2, 0xaf],
                [0x89, 0xb4, 0xfa],
                [0xf5, 0xc2, 0xe7],
                [0x94, 0xe2, 0xd5],
                [0xba, 0xc2, 0xde],
                [0x58, 0x5b, 0x70],
                [0xf3, 0x8b, 0xa8],
                [0xa6, 0xe3, 0xa1],
                [0xf9, 0xe2, 0xaf],
                [0x89, 0xb4, 0xfa],
                [0xf5, 0xc2, 0xe7],
                [0x94, 0xe2, 0xd5],
                [0xa6, 0xad, 0xc8],
            ],
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let (config, warnings) = parse("");
        assert!(warnings.is_empty());
        assert_eq!(config.font_size, 15.0);
        assert_eq!(config.background, DEFAULT_BG);
        assert_eq!(config.palette, PALETTE);
        assert_eq!(config.cursor_style, CursorStyle::Block);
    }

    #[test]
    fn parses_the_documented_keys() {
        let (config, warnings) = parse(
            r#"
            # a comment
            font-family = "Cascadia Mono"
            font-size = 18
            padding = 10
            cursor-style = bar
            scrollback-limit = 500
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.font_family.as_deref(), Some("Cascadia Mono"));
        assert_eq!(config.font_size, 18.0);
        assert_eq!(config.padding, 10.0);
        assert_eq!(config.cursor_style, CursorStyle::Bar);
        assert_eq!(config.scrollback, 500);
    }

    #[test]
    fn colours_accept_short_and_long_hex() {
        let (config, warnings) = parse("background = #abc\nforeground = \"#102030\"");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.background, [0xaa, 0xbb, 0xcc]);
        assert_eq!(config.foreground, [0x10, 0x20, 0x30]);
    }

    #[test]
    fn palette_entries_target_one_slot() {
        let (config, _) = parse("palette = 3=#ff0000");
        assert_eq!(config.palette[3], [0xff, 0x00, 0x00]);
        // The rest of the palette is untouched.
        assert_eq!(config.palette[1], PALETTE[1]);
    }

    #[test]
    fn themes_replace_colours() {
        let (config, warnings) = parse("theme = catppuccin-mocha");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.background, [0x1e, 0x1e, 0x2e]);
        assert_eq!(config.foreground, [0xcd, 0xd6, 0xf4]);
    }

    #[test]
    fn themes_apply_before_later_overrides() {
        let (config, _) = parse("theme = light\nbackground = #000000");
        assert_eq!(config.background, [0x00, 0x00, 0x00]);
        assert_eq!(config.foreground, [0x65, 0x7b, 0x83]);
    }

    #[test]
    fn bad_values_warn_instead_of_failing() {
        let (config, warnings) =
            parse("font-size = huge\nbackground = nope\npalette = 99=#ffffff\nnonsense-key = 1");
        assert_eq!(config.font_size, 15.0);
        assert_eq!(config.background, DEFAULT_BG);
        assert_eq!(warnings.len(), 4);
    }

    #[test]
    fn zero_scrollback_limit_is_rejected() {
        let (config, warnings) = parse("scrollback-limit = 0");
        assert_eq!(config.scrollback, SCROLLBACK_LIMIT);
        assert_eq!(warnings.len(), 1);
    }
}
