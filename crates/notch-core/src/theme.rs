//! Colour tokens. Resolved from the system light/dark setting, the user's config and the Windows
//! accent colour, so every module draws with semantic colours and never hard-codes values.

use serde::{Deserialize, Serialize};

use crate::color::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    /// Fill of the notch itself.
    pub bg: Color,
    /// Subtle raised surface on top of `bg` (cards, buttons).
    pub surface: Color,
    pub surface_hi: Color,
    pub text: Color,
    pub text_dim: Color,
    pub text_faint: Color,
    /// 1 px outline / separators.
    pub hairline: Color,
    pub accent: Color,
    /// Text/icon colour to put on top of `accent`.
    pub on_accent: Color,
    pub ok: Color,
    pub warn: Color,
    pub danger: Color,
}

/// Windows' default accent blue, used when the system value cannot be read.
pub const FALLBACK_ACCENT: Color = Color::rgba(0.0, 0.47, 0.83, 1.0);

impl Theme {
    pub fn dark(accent: Color) -> Theme {
        let bg = Color::BLACK;
        let accent = accent.with_alpha(1.0).ensure_contrast(bg, 4.5);
        Theme {
            dark: true,
            bg,
            surface: Color::rgba(1.0, 1.0, 1.0, 0.09),
            surface_hi: Color::rgba(1.0, 1.0, 1.0, 0.16),
            text: Color::WHITE,
            text_dim: Color::rgba(1.0, 1.0, 1.0, 0.64),
            text_faint: Color::rgba(1.0, 1.0, 1.0, 0.38),
            hairline: Color::rgba(1.0, 1.0, 1.0, 0.10),
            accent,
            on_accent: pick_on(accent),
            ok: Color::rgb8(0x32, 0xD7, 0x4B),
            warn: Color::rgb8(0xFF, 0xB3, 0x40),
            danger: Color::rgb8(0xFF, 0x45, 0x3A),
        }
    }

    pub fn light(accent: Color) -> Theme {
        let bg = Color::rgb8(0xF4, 0xF4, 0xF6);
        let accent = accent.with_alpha(1.0).ensure_contrast(bg, 3.0);
        Theme {
            dark: false,
            bg,
            surface: Color::rgba(0.0, 0.0, 0.0, 0.06),
            surface_hi: Color::rgba(0.0, 0.0, 0.0, 0.11),
            text: Color::rgb8(0x14, 0x14, 0x16),
            text_dim: Color::rgba(0.08, 0.08, 0.09, 0.62),
            text_faint: Color::rgba(0.08, 0.08, 0.09, 0.38),
            hairline: Color::rgba(0.0, 0.0, 0.0, 0.12),
            accent,
            on_accent: pick_on(accent),
            ok: Color::rgb8(0x1E, 0xA5, 0x3A),
            warn: Color::rgb8(0xC7, 0x7A, 0x00),
            danger: Color::rgb8(0xD7, 0x2B, 0x20),
        }
    }

    /// Resolve the effective theme.
    pub fn resolve(mode: ThemeMode, system_dark: bool, accent: Color) -> Theme {
        let dark = match mode {
            ThemeMode::System => system_dark,
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
        };
        if dark {
            Theme::dark(accent)
        } else {
            Theme::light(accent)
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark(FALLBACK_ACCENT)
    }
}

fn pick_on(accent: Color) -> Color {
    if accent.contrast(Color::BLACK) >= accent.contrast(Color::WHITE) {
        Color::BLACK
    } else {
        Color::WHITE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_is_always_legible_on_the_notch() {
        for hex in [
            "#0a143c", "#0078d4", "#ffffff", "#000000", "#f0f078", "#ff00ff",
        ] {
            let accent = Color::from_hex(hex).unwrap();
            let d = Theme::dark(accent);
            assert!(
                d.accent.contrast(d.bg) >= 4.4,
                "{hex} on dark {:?}",
                d.accent
            );
            let l = Theme::light(accent);
            assert!(
                l.accent.contrast(l.bg) >= 2.9,
                "{hex} on light {:?}",
                l.accent
            );
            assert!(
                d.on_accent.contrast(d.accent) >= 3.0,
                "{hex} label on accent"
            );
        }
    }

    #[test]
    fn resolve_follows_mode() {
        let a = FALLBACK_ACCENT;
        assert!(Theme::resolve(ThemeMode::System, true, a).dark);
        assert!(!Theme::resolve(ThemeMode::System, false, a).dark);
        assert!(Theme::resolve(ThemeMode::Dark, false, a).dark);
        assert!(!Theme::resolve(ThemeMode::Light, true, a).dark);
    }

    #[test]
    fn dark_notch_is_pure_black() {
        // The notch illusion depends on #000 matching the bezel; keep it exact.
        assert_eq!(Theme::dark(FALLBACK_ACCENT).bg, Color::BLACK);
    }
}
