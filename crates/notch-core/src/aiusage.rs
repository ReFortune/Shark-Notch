//! Claude Code's token use, read from the session logs it already writes
//! (`~/.claude/projects/**/*.jsonl`, one JSON object per line).
//!
//! Only the numbers are taken: the message id, the time and the `usage` counters. The text of a
//! conversation is parsed past and dropped, never kept. A message is logged more than once while it
//! streams (each line has the counters so far), so records are merged by message id, keeping the
//! largest figures.

use std::collections::HashMap;

use serde::Deserialize;

use crate::civil::unix_from_civil;

/// What one assistant message used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// UTC seconds since 1970.
    pub at: i64,
    /// New input: what was sent fresh plus what was written to the cache. (Cache reads are left
    /// out: they are the same context read again and dwarf everything else.)
    pub input: u64,
    pub output: u64,
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

/// `2026-10-09T03:25:21.027Z` as Unix seconds. Only UTC (`Z`) stamps are understood.
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u32>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, sec) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(unix_from_civil(y as i32, mo, d, h, mi, sec))
}

/// The usage in one log line, with the id of the message it belongs to. `None` for every line that
/// is not an assistant message with counters (most of them).
pub fn parse_line(line: &str) -> Option<(String, Record)> {
    // Cheap rejection first: the big lines (thinking, tool output) carry no counters.
    if !line.contains("\"usage\"") {
        return None;
    }
    let l: Line = serde_json::from_str(line).ok()?;
    let m = l.message?;
    let u = m.usage?;
    let at = parse_time(&l.timestamp?)?;
    let input = u.input_tokens.unwrap_or(0) + u.cache_creation_input_tokens.unwrap_or(0);
    Some((
        m.id?,
        Record {
            at,
            input,
            output: u.output_tokens.unwrap_or(0),
        },
    ))
}

/// What the notch shows: the totals since some moment (local midnight), and when the logs last grew.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub messages: u32,
    /// UTC seconds of the newest message (`0`: none).
    pub last_at: i64,
}

/// Every message seen, by id.
#[derive(Debug, Default)]
pub struct Ledger {
    by_id: HashMap<String, Record>,
}

impl Ledger {
    pub fn add(&mut self, id: String, r: Record) {
        let e = self.by_id.entry(id).or_insert(r);
        e.at = e.at.max(r.at);
        e.input = e.input.max(r.input);
        e.output = e.output.max(r.output);
    }

    /// Read a chunk of log text (whole lines), returning how many records it held.
    pub fn add_text(&mut self, text: &str) -> usize {
        let mut n = 0;
        for line in text.lines() {
            if let Some((id, r)) = parse_line(line) {
                self.add(id, r);
                n += 1;
            }
        }
        n
    }

    /// Totals of the messages at or after `since`.
    pub fn totals_since(&self, since: i64) -> Totals {
        let mut t = Totals::default();
        for r in self.by_id.values().filter(|r| r.at >= since) {
            t.input += r.input;
            t.output += r.output;
            t.messages += 1;
            t.last_at = t.last_at.max(r.at);
        }
        t
    }

    /// Forget what is older than `before` (the ledger only ever needs today).
    pub fn prune(&mut self, before: i64) {
        self.by_id.retain(|_, r| r.at >= before);
    }
}

/// `1234` → "1.2k", `1_500_000` → "1.5M".
pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: &str = r#"{"type":"assistant","timestamp":"2026-10-09T03:25:21.027Z","message":{"id":"msg_a","role":"assistant","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":10}}}"#;
    const END: &str = r#"{"type":"assistant","timestamp":"2026-10-09T03:25:25.500Z","message":{"id":"msg_a","usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":145}}}"#;

    #[test]
    fn time_stamps_are_read_as_utc() {
        assert_eq!(parse_time("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_time("2026-10-09T03:25:21.027Z"),
            Some(unix_from_civil(2026, 10, 9, 3, 25, 21))
        );
        for bad in [
            "",
            "2026-10-09",
            "2026-10-09T03:25:21+02:00",
            "2026-13-09T03:25:21Z",
            "x",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_usage_line_gives_its_counters_and_other_lines_give_nothing() {
        let (id, r) = parse_line(START).unwrap();
        assert_eq!(id, "msg_a");
        assert_eq!((r.input, r.output), (102, 10));
        for other in [
            "",
            "not json",
            r#"{"type":"user","timestamp":"2026-10-09T03:25:21.027Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"timestamp":"2026-10-09T03:25:21.027Z","message":{"id":"m","usage":{"output_tokens":1}},"x":"#,
        ] {
            assert_eq!(parse_line(other), None, "{other}");
        }
    }

    #[test]
    fn a_streamed_message_counts_once_with_its_final_figures() {
        let mut l = Ledger::default();
        assert_eq!(l.add_text(&format!("{START}\n{END}\n")), 2);
        let t = l.totals_since(0);
        assert_eq!((t.messages, t.input, t.output), (1, 102, 145));
        // Reading the same text again changes nothing.
        l.add_text(START);
        assert_eq!(l.totals_since(0), t);
    }

    #[test]
    fn totals_cover_only_today_and_old_messages_can_be_dropped() {
        let mut l = Ledger::default();
        l.add_text(START);
        let day = unix_from_civil(2026, 10, 9, 0, 0, 0);
        assert_eq!(l.totals_since(day).messages, 1);
        assert_eq!(
            l.totals_since(day + 86_400).messages,
            0,
            "tomorrow starts empty"
        );
        assert_eq!(
            l.totals_since(day).last_at,
            unix_from_civil(2026, 10, 9, 3, 25, 21)
        );
        l.prune(day + 86_400);
        assert_eq!(l.totals_since(0).messages, 0);
    }

    #[test]
    fn token_counts_read_naturally() {
        for (n, s) in [
            (0, "0"),
            (999, "999"),
            (1000, "1k"),
            (1234, "1.2k"),
            (84_000, "84k"),
            (1_500_000, "1.5M"),
            (2_000_000, "2M"),
        ] {
            assert_eq!(fmt_tokens(n), s);
        }
    }
}
