//! Straight-alpha sRGB colours. Renderers premultiply when they need to.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color::rgba(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Color = Color::rgba(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Color = Color::rgba(1.0, 1.0, 1.0, 1.0);

    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn rgb8(r: u8, g: u8, b: u8) -> Self {
        Self::rgba8(r, g, b, 255)
    }

    pub fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::rgba(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a as f32 / 255.0,
        )
    }

    /// Parse `#RGB`, `#RRGGBB` or `#RRGGBBAA` (the `#` is optional).
    pub fn from_hex(s: &str) -> Option<Color> {
        let s = s.trim().trim_start_matches('#');
        if !s.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        match s.len() {
            3 => {
                let n = |i: usize| u8::from_str_radix(&s[i..i + 1], 16).ok().map(|v| v * 17);
                Some(Color::rgb8(n(0)?, n(1)?, n(2)?))
            }
            6 => Some(Color::rgb8(byte(0)?, byte(2)?, byte(4)?)),
            8 => Some(Color::rgba8(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => None,
        }
    }

    pub fn with_alpha(self, a: f32) -> Color {
        Color { a, ..self }
    }

    /// Multiply alpha (used for group fades).
    pub fn mul_alpha(self, k: f32) -> Color {
        Color {
            a: (self.a * k).clamp(0.0, 1.0),
            ..self
        }
    }

    pub fn lerp(self, o: Color, t: f32) -> Color {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Color::rgba(
            l(self.r, o.r),
            l(self.g, o.g),
            l(self.b, o.b),
            l(self.a, o.a),
        )
    }

    /// Premultiplied components `[r, g, b, a]`.
    pub fn premultiplied(self) -> [f32; 4] {
        [self.r * self.a, self.g * self.a, self.b * self.a, self.a]
    }

    pub fn to_rgba8(self) -> [u8; 4] {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        [q(self.r), q(self.g), q(self.b), q(self.a)]
    }

    /// WCAG relative luminance of the colour (alpha ignored).
    pub fn luminance(self) -> f32 {
        let lin = |c: f32| {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// WCAG contrast ratio between two colours (1..21).
    pub fn contrast(self, o: Color) -> f32 {
        let (a, b) = (self.luminance(), o.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Nudge towards white or black until it has at least `min` contrast against `bg`.
    pub fn ensure_contrast(self, bg: Color, min: f32) -> Color {
        if self.contrast(bg) >= min {
            return self;
        }
        let towards = if bg.luminance() < 0.5 {
            Color::WHITE
        } else {
            Color::BLACK
        };
        let mut c = self;
        for i in 1..=20 {
            c = self.lerp(towards, i as f32 * 0.05).with_alpha(self.a);
            if c.contrast(bg) >= min {
                break;
            }
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(Color::from_hex("#fff"), Some(Color::WHITE));
        assert_eq!(Color::from_hex("000000"), Some(Color::BLACK));
        assert_eq!(
            Color::from_hex("#FF000080").map(|c| c.to_rgba8()),
            Some([255, 0, 0, 128])
        );
        assert_eq!(Color::from_hex("#12"), None);
        assert_eq!(Color::from_hex("#gg0000"), None);
        assert_eq!(Color::from_hex("#é12345"), None, "non-ascii must not panic");
    }

    #[test]
    fn luminance_and_contrast() {
        assert!((Color::BLACK.contrast(Color::WHITE) - 21.0).abs() < 1e-3);
        assert!(Color::WHITE.luminance() > 0.99);
        assert_eq!(Color::BLACK.luminance(), 0.0);
    }

    #[test]
    fn ensure_contrast_lifts_dark_accents_on_black() {
        let navy = Color::rgb8(10, 20, 60);
        let fixed = navy.ensure_contrast(Color::BLACK, 4.5);
        assert!(fixed.contrast(Color::BLACK) >= 4.5, "{fixed:?}");
        let already = Color::rgb8(255, 200, 0);
        assert_eq!(already.ensure_contrast(Color::BLACK, 4.5), already);
        let pale = Color::rgb8(240, 240, 120);
        assert!(
            pale.ensure_contrast(Color::WHITE, 3.0)
                .contrast(Color::WHITE)
                >= 3.0
        );
    }

    #[test]
    fn alpha_helpers() {
        let c = Color::rgba(1.0, 0.5, 0.0, 0.5);
        assert_eq!(c.premultiplied(), [0.5, 0.25, 0.0, 0.5]);
        assert_eq!(c.mul_alpha(0.5).a, 0.25);
        assert_eq!(c.mul_alpha(10.0).a, 1.0);
        assert_eq!(Color::BLACK.lerp(Color::WHITE, 0.5).r, 0.5);
    }
}
