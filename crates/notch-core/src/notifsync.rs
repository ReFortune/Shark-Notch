//! Text cleaning for banners that come from outside (the iPhone link): single line, no control
//! characters, bounded, so a sender can never make the notch draw an unreasonable amount of text.

/// Collapse whitespace and drop control characters, then cut to `max` characters with an ellipsis.
pub fn tidy(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    let mut pending_space = false;
    let mut count = 0usize;
    for c in s.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() {
            continue;
        }
        if count >= max {
            // One more printable character exists beyond the limit.
            out.push('…');
            return out;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
            count += 1;
            if count >= max {
                out.push('…');
                return out;
            }
        }
        out.push(c);
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_single_line_and_free_of_control_characters() {
        assert_eq!(tidy("Line\none\t two\u{7}", 100), "Line one two");
        assert_eq!(tidy("  a\r\n\r\nb  ", 100), "a b");
    }

    #[test]
    fn long_text_is_cut_with_an_ellipsis_at_a_character_boundary() {
        assert_eq!(tidy(&"é".repeat(500), 120).chars().count(), 121);
        let exact = "x".repeat(120);
        assert_eq!(tidy(&exact, 120), exact);
        assert_eq!(tidy(&"x".repeat(121), 120), format!("{}…", exact));
    }

    #[test]
    fn trailing_whitespace_does_not_leave_a_dangling_ellipsis() {
        let text = format!("{}   ", "y".repeat(120));
        assert_eq!(tidy(&text, 120), "y".repeat(120));
    }
}
