//! Lyrics from LRCLIB (`https://lrclib.net`), the pure part: building the request, reading the
//! answer and finding the line that goes with a playback position.
//!
//! Opt-in (`[media] lyrics`, off by default). One request per track, only while the Media page is
//! open. Only timed ("synced") lyrics are shown; the text is kept in memory for the current track
//! and never written to disk. LRCLIB publishes no terms that could be verified (see
//! `docs/MEDIA.md`), which is why this is off unless you turn it on.

use std::sync::Arc;

use serde::Deserialize;

/// Longest lyrics kept, in lines and in characters per line (a bound on what a server can make us hold).
pub const MAX_LINES: usize = 600;
pub const MAX_LINE_CHARS: usize = 160;

/// What is known about the lyrics of the current track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lyrics {
    /// Timed lines: `(start in ms, text)`, in order.
    Synced(Vec<(u64, String)>),
    /// The service has the words but no timing.
    Untimed,
    Instrumental,
    NotFound,
    /// The request failed (offline, a server error, a refusal): shown as nothing, tried again on
    /// the next track.
    Failed,
}

/// The answer to one request, tagged with the track it was for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LyricsReply {
    /// `artist\ntitle` as the module asked.
    pub track: Arc<str>,
    pub lyrics: Lyrics,
}

/// The key a track is asked for and matched by.
pub fn track_key(artist: &str, title: &str) -> String {
    format!("{artist}\n{title}")
}

/// Percent-encode a query value (RFC 3986 unreserved characters stay).
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `/api/get?...` for an exact match (album and duration help it find the right recording).
pub fn get_path(artist: &str, title: &str, album: &str, duration_s: u32) -> String {
    let mut p = format!(
        "/api/get?artist_name={}&track_name={}",
        pct_encode(artist),
        pct_encode(title)
    );
    if !album.is_empty() {
        p.push_str(&format!("&album_name={}", pct_encode(album)));
    }
    if duration_s > 0 {
        p.push_str(&format!("&duration={duration_s}"));
    }
    p
}

/// `/api/search?...`, the looser fallback when the exact lookup finds nothing.
pub fn search_path(artist: &str, title: &str) -> String {
    format!(
        "/api/search?artist_name={}&track_name={}",
        pct_encode(artist),
        pct_encode(title)
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Entry {
    instrumental: bool,
    #[serde(rename = "syncedLyrics")]
    synced: Option<String>,
    #[serde(rename = "plainLyrics")]
    plain: Option<String>,
}

fn from_entry(e: &Entry) -> Lyrics {
    if e.instrumental {
        return Lyrics::Instrumental;
    }
    if let Some(s) = e.synced.as_deref() {
        let lines = parse_lrc(s);
        if !lines.is_empty() {
            return Lyrics::Synced(lines);
        }
    }
    if e.plain.as_deref().is_some_and(|p| !p.trim().is_empty()) {
        Lyrics::Untimed
    } else {
        Lyrics::NotFound
    }
}

/// The answer of `/api/get` (one object).
pub fn parse_get(body: &[u8]) -> Lyrics {
    serde_json::from_slice::<Entry>(body).map_or(Lyrics::Failed, |e| from_entry(&e))
}

/// The answer of `/api/search` (an array): the first entry that has timed lyrics, else the first
/// that has words at all.
pub fn parse_search(body: &[u8]) -> Lyrics {
    let Ok(all) = serde_json::from_slice::<Vec<Entry>>(body) else {
        return Lyrics::Failed;
    };
    let mut best = Lyrics::NotFound;
    for e in &all {
        match from_entry(e) {
            l @ Lyrics::Synced(_) => return l,
            Lyrics::Untimed | Lyrics::Instrumental if best == Lyrics::NotFound => {
                best = from_entry(e);
            }
            _ => {}
        }
    }
    best
}

/// `[mm:ss.xx] words` lines. A line may carry several time stamps; metadata tags (`[ar:...]`) and
/// anything unreadable are skipped. Sorted by time, bounded.
pub fn parse_lrc(text: &str) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else { break };
            match parse_stamp(&r[..end]) {
                Some(ms) => stamps.push(ms),
                None => break,
            }
            rest = r[end + 1..].trim_start();
        }
        if stamps.is_empty() {
            continue;
        }
        let words: String = rest.chars().take(MAX_LINE_CHARS).collect();
        for ms in stamps {
            out.push((ms, words.trim().to_string()));
        }
        if out.len() >= MAX_LINES {
            break;
        }
    }
    out.sort_by_key(|l| l.0);
    out.truncate(MAX_LINES);
    out
}

/// `mm:ss`, `mm:ss.x`, `mm:ss.xx` or `mm:ss.xxx` in milliseconds.
fn parse_stamp(s: &str) -> Option<u64> {
    let (m, rest) = s.split_once(':')?;
    let (sec, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let m: u64 = m.parse().ok()?;
    let sec: u64 = sec.parse().ok()?;
    if sec >= 60 || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac_ms = if frac.is_empty() {
        0
    } else {
        frac.parse::<u64>().ok()? * 10u64.pow(3 - frac.len() as u32)
    };
    Some((m * 60 + sec) * 1000 + frac_ms)
}

/// Index of the line being sung at `pos_ms` (the last one that has started), if any has.
pub fn current(lines: &[(u64, String)], pos_ms: u64) -> Option<usize> {
    lines.partition_point(|l| l.0 <= pos_ms).checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LRC: &str = "[ar:Someone]\n[00:12.00] first line\n[00:15.5]second\n[01:02.345] third [x]\n[00:01.00][00:20.00] chorus\nplain words\n[bad] nope";

    #[test]
    fn lrc_lines_are_read_in_time_order_and_metadata_is_skipped() {
        let l = parse_lrc(LRC);
        let texts: Vec<&str> = l.iter().map(|x| x.1.as_str()).collect();
        assert_eq!(
            texts,
            ["chorus", "first line", "second", "chorus", "third [x]"]
        );
        assert_eq!(l[0].0, 1_000);
        assert_eq!(l[2].0, 15_500);
        assert_eq!(l[4].0, 62_345);
    }

    #[test]
    fn stamps_are_strict() {
        assert_eq!(parse_stamp("00:01"), Some(1_000));
        assert_eq!(parse_stamp("02:03.4"), Some(123_400));
        assert_eq!(parse_stamp("02:03.45"), Some(123_450));
        assert_eq!(parse_stamp("02:03.456"), Some(123_456));
        for bad in ["", "ar:x", "00:61", "00:01.1234", "00:01.x", "-1:00", "1"] {
            assert_eq!(parse_stamp(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_current_line_is_the_last_one_that_has_started() {
        let l = parse_lrc("[00:10.00] a\n[00:20.00] b\n[00:30.00] c");
        assert_eq!(current(&l, 0), None, "before the first line");
        assert_eq!(current(&l, 10_000), Some(0));
        assert_eq!(current(&l, 19_999), Some(0));
        assert_eq!(current(&l, 20_000), Some(1));
        assert_eq!(current(&l, 10_000_000), Some(2));
        assert_eq!(current(&[], 5), None);
    }

    #[test]
    fn lines_are_bounded() {
        let long = format!("[00:01.00] {}", "x".repeat(1000));
        assert_eq!(parse_lrc(&long)[0].1.chars().count(), MAX_LINE_CHARS);
        let many: String = (0..5000)
            .map(|i| format!("[{:02}:{:02}.00] w\n", i / 60 % 60, i % 60))
            .collect();
        assert_eq!(parse_lrc(&many).len(), MAX_LINES);
    }

    #[test]
    fn answers_are_classified() {
        let synced = br#"{"instrumental":false,"plainLyrics":"a","syncedLyrics":"[00:01.00] a"}"#;
        assert!(matches!(parse_get(synced), Lyrics::Synced(l) if l.len() == 1));
        let plain = br#"{"plainLyrics":"words","syncedLyrics":null}"#;
        assert_eq!(parse_get(plain), Lyrics::Untimed);
        assert_eq!(parse_get(br#"{"instrumental":true}"#), Lyrics::Instrumental);
        assert_eq!(
            parse_get(br#"{"plainLyrics":"  ","syncedLyrics":""}"#),
            Lyrics::NotFound
        );
        assert_eq!(parse_get(b"<html>"), Lyrics::Failed);
        // A search prefers a timed entry over an earlier untimed one.
        let search = br#"[{"plainLyrics":"w"},{"syncedLyrics":"[00:02.00] b"}]"#;
        assert!(matches!(parse_search(search), Lyrics::Synced(_)));
        assert_eq!(parse_search(br#"[{"plainLyrics":"w"}]"#), Lyrics::Untimed);
        assert_eq!(parse_search(b"[]"), Lyrics::NotFound);
        assert_eq!(parse_search(b"{}"), Lyrics::Failed);
    }

    #[test]
    fn requests_are_percent_encoded() {
        assert_eq!(pct_encode("AC/DC & Co é"), "AC%2FDC%20%26%20Co%20%C3%A9");
        assert_eq!(
            get_path("Björk", "Hyper-ballad", "", 0),
            "/api/get?artist_name=Bj%C3%B6rk&track_name=Hyper-ballad"
        );
        assert_eq!(
            get_path("A", "B", "C D", 215),
            "/api/get?artist_name=A&track_name=B&album_name=C%20D&duration=215"
        );
        assert_eq!(
            search_path("A", "B"),
            "/api/search?artist_name=A&track_name=B"
        );
        assert_eq!(track_key("A", "B"), "A\nB");
    }
}
