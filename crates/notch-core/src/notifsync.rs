//! The bookkeeping behind a notification listener, kept free of the OS so it can be tested anywhere.
//!
//! Windows tells the listener *that something changed*, not reliably *what*; the robust way to
//! follow it is to read the whole list after each change and compare it with what was announced
//! before. [`Tracker`] does that comparison. [`compose_text`] turns the text lines of a toast into
//! the title/body pair the notch shows, bounded so an app can never make the notch allocate or draw
//! an unreasonable amount of text.

use std::collections::HashSet;

/// Longest title / body kept, in characters.
pub const MAX_TITLE: usize = 120;
pub const MAX_BODY: usize = 300;

/// One notification as the platform currently lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub id: u64,
    /// Any ordering key that grows with time (Windows `FILETIME` ticks).
    pub created: i64,
}

/// What changed since the previous [`Tracker::update`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Update {
    /// Newly appeared notifications, **oldest first**, with whether each should be announced.
    /// The start-up backlog is listed but never announced (`fresh == false`).
    pub added: Vec<(u64, bool)>,
    /// Notifications that are gone from the platform's list.
    pub removed: Vec<u64>,
}

/// Remembers which notifications were already handed to the bus.
#[derive(Debug)]
pub struct Tracker {
    known: HashSet<u64>,
    primed: bool,
    backlog_limit: usize,
}

impl Tracker {
    /// `backlog_limit`: how many of the notifications already waiting at start-up to list (newest).
    pub fn new(backlog_limit: usize) -> Tracker {
        Tracker {
            known: HashSet::new(),
            primed: false,
            backlog_limit,
        }
    }

    /// Forget everything (access was revoked and came back): the next update is a backlog again.
    pub fn reset(&mut self) {
        self.known.clear();
        self.primed = false;
    }

    /// Compare the platform's current list with what was seen before.
    pub fn update(&mut self, present: &[Seen]) -> Update {
        let mut sorted: Vec<Seen> = present.to_vec();
        sorted.sort_by_key(|s| (s.created, s.id));
        let now_ids: HashSet<u64> = sorted.iter().map(|s| s.id).collect();

        let mut up = Update::default();
        if !self.primed {
            self.primed = true;
            let skip = sorted.len().saturating_sub(self.backlog_limit);
            up.added = sorted.iter().skip(skip).map(|s| (s.id, false)).collect();
        } else {
            up.added = sorted
                .iter()
                .filter(|s| !self.known.contains(&s.id))
                .map(|s| (s.id, true))
                .collect();
            up.removed = self
                .known
                .iter()
                .filter(|id| !now_ids.contains(id))
                .copied()
                .collect();
            up.removed.sort_unstable();
        }
        self.known = now_ids;
        up
    }
}

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

/// Title and body from a toast's text elements: the first non-empty line is the title, the rest
/// (joined by spaces) the body. Both are single-line and length-bounded.
pub fn compose_text<S: AsRef<str>>(lines: &[S]) -> (String, String) {
    let mut it = lines
        .iter()
        .map(|l| l.as_ref().trim())
        .filter(|l| !l.is_empty());
    let title = tidy(it.next().unwrap_or(""), MAX_TITLE);
    let rest: Vec<&str> = it.collect();
    let body = tidy(&rest.join(" "), MAX_BODY);
    (title, body)
}

/// Whole seconds between two `FILETIME` tick counts (100 ns units), never negative, saturating.
pub fn age_secs(created_ticks: i64, now_ticks: i64) -> u32 {
    let d = now_ticks.saturating_sub(created_ticks).max(0) / 10_000_000;
    u32::try_from(d).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: u64, created: i64) -> Seen {
        Seen { id, created }
    }

    #[test]
    fn the_first_read_is_a_silent_backlog_oldest_first() {
        let mut t = Tracker::new(10);
        let up = t.update(&[s(3, 300), s(1, 100), s(2, 200)]);
        assert_eq!(up.added, vec![(1, false), (2, false), (3, false)]);
        assert!(up.removed.is_empty());
    }

    #[test]
    fn the_backlog_is_limited_to_the_newest() {
        let mut t = Tracker::new(2);
        let up = t.update(&[s(1, 10), s(2, 20), s(3, 30), s(4, 40)]);
        assert_eq!(up.added, vec![(3, false), (4, false)]);
        // The ones that were cut are still *known*: they are never announced later.
        let up = t.update(&[s(1, 10), s(2, 20), s(3, 30), s(4, 40)]);
        assert_eq!(up, Update::default());
    }

    #[test]
    fn later_arrivals_are_announced_oldest_first_and_vanished_ones_reported() {
        let mut t = Tracker::new(10);
        t.update(&[s(1, 10), s(2, 20)]);
        let up = t.update(&[s(2, 20), s(5, 50), s(4, 40)]);
        assert_eq!(up.added, vec![(4, true), (5, true)]);
        assert_eq!(up.removed, vec![1]);
        let up = t.update(&[s(2, 20), s(5, 50), s(4, 40)]);
        assert_eq!(up, Update::default(), "nothing changed, nothing sent");
    }

    #[test]
    fn a_replacement_with_a_new_id_is_one_removal_and_one_arrival() {
        let mut t = Tracker::new(10);
        t.update(&[s(7, 10)]);
        let up = t.update(&[s(8, 20)]);
        assert_eq!((up.added, up.removed), (vec![(8, true)], vec![7]));
    }

    #[test]
    fn an_empty_first_read_still_primes_the_tracker() {
        let mut t = Tracker::new(10);
        assert_eq!(t.update(&[]), Update::default());
        let up = t.update(&[s(1, 1)]);
        assert_eq!(
            up.added,
            vec![(1, true)],
            "after the first read, arrivals are fresh"
        );
    }

    #[test]
    fn resetting_makes_the_next_read_a_backlog_again() {
        let mut t = Tracker::new(10);
        t.update(&[s(1, 1)]);
        t.reset();
        let up = t.update(&[s(1, 1), s(2, 2)]);
        assert_eq!(up.added, vec![(1, false), (2, false)]);
        assert!(up.removed.is_empty());
    }

    #[test]
    fn equal_timestamps_order_by_id_so_results_are_deterministic() {
        let mut t = Tracker::new(10);
        t.update(&[]);
        let up = t.update(&[s(9, 5), s(3, 5), s(6, 5)]);
        assert_eq!(up.added, vec![(3, true), (6, true), (9, true)]);
    }

    #[test]
    fn first_line_is_the_title_and_the_rest_the_body() {
        let (t, b) = compose_text(&["Alice", "  are you there?  ", "", "call me"]);
        assert_eq!(t, "Alice");
        assert_eq!(b, "are you there? call me");
        assert_eq!(compose_text::<&str>(&[]), (String::new(), String::new()));
        assert_eq!(
            compose_text(&["", "  ", "Only"]),
            ("Only".to_string(), String::new())
        );
    }

    #[test]
    fn text_is_single_line_and_free_of_control_characters() {
        let (t, b) = compose_text(&["Line\none\t two\u{7}", "a\r\n\r\nb"]);
        assert_eq!(t, "Line one two");
        assert_eq!(b, "a b");
    }

    #[test]
    fn long_text_is_cut_with_an_ellipsis_at_a_character_boundary() {
        let long = "é".repeat(500);
        let (t, b) = compose_text(&[long.as_str(), long.as_str()]);
        assert_eq!(t.chars().count(), MAX_TITLE + 1);
        assert!(t.ends_with('…'));
        assert_eq!(b.chars().count(), MAX_BODY + 1);
        // Exactly at the limit: untouched.
        let exact = "x".repeat(MAX_TITLE);
        assert_eq!(compose_text(&[exact.as_str()]).0, exact);
        let over = "x".repeat(MAX_TITLE + 1);
        assert_eq!(
            compose_text(&[over.as_str()]).0,
            format!("{}…", "x".repeat(MAX_TITLE))
        );
    }

    #[test]
    fn trailing_whitespace_does_not_leave_a_dangling_ellipsis() {
        let text = format!("{}   ", "y".repeat(MAX_TITLE));
        assert_eq!(compose_text(&[text.as_str()]).0, "y".repeat(MAX_TITLE));
    }

    #[test]
    fn ages_are_whole_seconds_and_never_negative() {
        let sec = 10_000_000;
        assert_eq!(age_secs(100 * sec, 130 * sec + 5), 30);
        assert_eq!(age_secs(130 * sec, 100 * sec), 0, "clock skew");
        assert_eq!(age_secs(i64::MIN, i64::MAX), u32::MAX);
    }
}
