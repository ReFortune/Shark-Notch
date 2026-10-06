//! Parse hotkey strings such as `"Ctrl+Alt+N"` into the numbers `RegisterHotKey` wants.
//!
//! The modifier and virtual-key values are Win32's (they are stable ABI constants), kept here as
//! plain numbers so parsing is portable and testable.

pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyError {
    NoKey,
    UnknownKey(String),
    TooManyKeys,
    /// A bare letter/digit/etc. without a modifier would steal normal typing.
    NeedsModifier,
}

impl std::fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HotkeyError::NoKey => write!(f, "hotkey has modifiers but no key"),
            HotkeyError::UnknownKey(k) => write!(f, "unknown key '{k}'"),
            HotkeyError::TooManyKeys => write!(f, "hotkey has more than one non-modifier key"),
            HotkeyError::NeedsModifier => write!(
                f,
                "hotkey needs at least one modifier (Ctrl/Alt/Shift/Win) unless it is a function key"
            ),
        }
    }
}

/// Parse a hotkey. An empty/whitespace string means "disabled" and yields `Ok(None)`.
pub fn parse(s: &str) -> Result<Option<Hotkey>, HotkeyError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let mut mods = 0;
    let mut key: Option<u32> = None;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" | "ctl" => mods |= MOD_CONTROL,
            "alt" | "option" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "windows" | "super" | "meta" | "cmd" => mods |= MOD_WIN,
            _ => {
                let vk =
                    key_code(&lower).ok_or_else(|| HotkeyError::UnknownKey(part.to_string()))?;
                if key.replace(vk).is_some() {
                    return Err(HotkeyError::TooManyKeys);
                }
            }
        }
    }
    let vk = key.ok_or(HotkeyError::NoKey)?;
    let is_fkey = (0x70..=0x87).contains(&vk);
    if mods == 0 && !is_fkey {
        return Err(HotkeyError::NeedsModifier);
    }
    Ok(Some(Hotkey { mods, vk }))
}

fn key_code(name: &str) -> Option<u32> {
    let b = name.as_bytes();
    if b.len() == 1 {
        match b[0] {
            c @ b'a'..=b'z' => return Some(c.to_ascii_uppercase() as u32),
            c @ b'0'..=b'9' => return Some(c as u32),
            _ => {}
        }
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=24).contains(&n)
    {
        return Some(0x70 + n - 1);
    }
    Some(match name {
        "space" | "spacebar" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" | "back" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "`" | "backtick" | "grave" | "tilde" => 0xC0,
        "-" | "minus" => 0xBD,
        "=" | "equals" | "plus" => 0xBB,
        "[" => 0xDB,
        "]" => 0xDD,
        "\\" | "backslash" => 0xDC,
        ";" | "semicolon" => 0xBA,
        "'" | "quote" => 0xDE,
        "," | "comma" => 0xBC,
        "." | "period" => 0xBE,
        "/" | "slash" => 0xBF,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(
            parse("Ctrl+Alt+N"),
            Ok(Some(Hotkey {
                mods: MOD_CONTROL | MOD_ALT,
                vk: b'N' as u32
            }))
        );
        assert_eq!(
            parse("  win + shift + space "),
            Ok(Some(Hotkey {
                mods: MOD_WIN | MOD_SHIFT,
                vk: 0x20
            }))
        );
        assert_eq!(
            parse("ctrl+F12"),
            Ok(Some(Hotkey {
                mods: MOD_CONTROL,
                vk: 0x7B
            }))
        );
        assert_eq!(
            parse("F13"),
            Ok(Some(Hotkey { mods: 0, vk: 0x7C })),
            "bare function keys are allowed"
        );
        assert_eq!(
            parse("Ctrl+`"),
            Ok(Some(Hotkey {
                mods: MOD_CONTROL,
                vk: 0xC0
            }))
        );
        assert_eq!(
            parse("alt+9"),
            Ok(Some(Hotkey {
                mods: MOD_ALT,
                vk: b'9' as u32
            }))
        );
    }

    #[test]
    fn empty_means_disabled() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("   "), Ok(None));
    }

    #[test]
    fn rejects_bad_input_with_useful_errors() {
        assert_eq!(parse("Ctrl+Alt"), Err(HotkeyError::NoKey));
        assert_eq!(
            parse("Ctrl+Banana"),
            Err(HotkeyError::UnknownKey("Banana".into()))
        );
        assert_eq!(parse("Ctrl+A+B"), Err(HotkeyError::TooManyKeys));
        assert_eq!(
            parse("N"),
            Err(HotkeyError::NeedsModifier),
            "would hijack typing"
        );
        assert_eq!(parse("F25"), Err(HotkeyError::UnknownKey("F25".into())));
        assert!(
            parse("Ctrl+Banana")
                .unwrap_err()
                .to_string()
                .contains("Banana")
        );
    }
}
