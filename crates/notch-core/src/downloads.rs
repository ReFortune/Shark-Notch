//! Telling downloads from the files browsers leave in the Downloads folder.
//!
//! Browsers write a download into a *partial* file (`name.zip.crdownload`, `name.zip.part`, ...) and
//! rename it to its real name when it is complete. That is observable from the file system alone —
//! no hooks, no browser integration — but it is all the file system says: the final size is not
//! known in advance, so there is no percentage, only "so much, so fast". This tracker turns the
//! raw changes (a partial file appeared / grew / was renamed / vanished) into those facts.

use std::collections::HashMap;

/// Extensions browsers use for a download in progress.
pub const PARTIAL_EXTENSIONS: &[&str] =
    &["crdownload", "part", "partial", "download", "opdownload"];

/// `foo.zip` for `foo.zip.crdownload`; `None` if this is not a partial file.
pub fn partial_base(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    let base = &name[..dot];
    (!base.is_empty()
        && PARTIAL_EXTENSIONS
            .iter()
            .any(|e| ext.eq_ignore_ascii_case(e)))
    .then_some(base)
}

/// One entry of a `ReadDirectoryChangesW` result, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Added(String),
    Removed(String),
    Modified(String),
    RenamedFrom(String),
    RenamedTo(String),
}

/// Decode the buffer `ReadDirectoryChangesW` fills: a chain of `FILE_NOTIFY_INFORMATION` records
/// (`next offset`, `action`, `name length in bytes`, then the UTF-16 name, all little-endian).
/// Malformed or truncated input yields the records that were intact and never panics: the buffer
/// is written by the kernel, but this does not trust it to be well-formed.
pub fn parse_changes(buf: &[u8]) -> Vec<Change> {
    let mut out = Vec::new();
    let mut off = 0usize;
    let word = |at: usize| -> Option<u32> {
        let b = buf.get(at..at.checked_add(4)?)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    while let (Some(next), Some(action), Some(len)) = (word(off), word(off + 4), word(off + 8)) {
        let start = off + 12;
        let Some(raw) = start
            .checked_add(len as usize)
            .and_then(|end| buf.get(start..end))
        else {
            break;
        };
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let name = String::from_utf16_lossy(&units);
        match action {
            1 => out.push(Change::Added(name)),
            2 => out.push(Change::Removed(name)),
            3 => out.push(Change::Modified(name)),
            4 => out.push(Change::RenamedFrom(name)),
            5 => out.push(Change::RenamedTo(name)),
            _ => {}
        }
        if next == 0 {
            break;
        }
        off = match off.checked_add(next as usize) {
            Some(o) => o,
            None => break,
        };
    }
    out
}

/// Is this a plain absolute Windows path (`C:\dir\file`, `\\server\share\file`)? "Show in folder"
/// is only ever given such a path: never a URL, a device path (`\\?\`, `\\.\`), a shell namespace
/// name (`::{...}`), a relative path or anything with control characters.
pub fn is_plain_absolute_path(p: &str) -> bool {
    if p.is_empty() || p.len() > 32_000 || p.chars().any(char::is_control) {
        return false;
    }
    let b = p.as_bytes();
    if b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
        return true;
    }
    if let Some(rest) = p.strip_prefix("\\\\") {
        let mut parts = rest.split('\\');
        let server = parts.next().unwrap_or("");
        let share = parts.next().unwrap_or("");
        return !server.is_empty() && !share.is_empty() && server != "?" && server != ".";
    }
    false
}

/// A file name made safe to show: no control characters, no characters that reorder text (a name
/// like `invoice\u{202E}fdp.exe` would otherwise read as a PDF), at most 80 characters.
pub fn display_name(name: &str) -> String {
    const BIDI: &[char] = &[
        '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
        '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
    ];
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && !BIDI.contains(c))
        .collect();
    if clean.chars().count() <= 80 {
        return clean;
    }
    // Keep the end: the extension is the part that matters.
    let tail: String = clean
        .chars()
        .rev()
        .take(76)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// Longest gap between samples that is averaged into the speed.
const SPEED_WINDOW: f64 = 0.4;
/// How much a new sample moves the smoothed speed.
const ALPHA: f64 = 0.35;
/// A download that has not grown for this long shows no speed.
const STALL_AFTER: f64 = 2.5;
/// A partial file that has not grown for this long is no longer listed (a browser that crashed
/// leaves its partial files behind; a download paused for a while is still listed).
pub const ABANDON_AFTER: f64 = 600.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Active {
    /// The name the file will have when it is done.
    pub name: String,
    pub bytes: u64,
    pub speed_bps: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// A download began (or the first sample of it arrived).
    Started(String),
    /// Something changed in the list of active downloads.
    Changed,
    /// The partial file was renamed to `name`: it is complete (`bytes` as last seen).
    Finished { name: String, bytes: u64 },
    /// The partial file vanished without becoming its final name.
    Cancelled(String),
}

#[derive(Clone, Debug)]
struct State {
    base: String,
    bytes: u64,
    last_t: f64,
    last_bytes: u64,
    speed: f64,
    /// When the file last grew (or was first seen).
    last_change: f64,
}

/// Active downloads, keyed by the partial file's own name.
#[derive(Debug, Default)]
pub struct Tracker {
    active: HashMap<String, State>,
    /// The old name of a rename whose new name has not arrived yet (the pair can straddle two reads).
    rename_from: Option<String>,
}

impl Tracker {
    /// A partial file exists with `size` bytes at time `now` (seconds, monotonic).
    pub fn partial_seen(&mut self, partial: &str, size: u64, now: f64) -> Vec<Update> {
        let Some(base) = partial_base(partial) else {
            return Vec::new();
        };
        match self.active.get_mut(partial) {
            None => {
                self.active.insert(
                    partial.to_string(),
                    State {
                        base: base.to_string(),
                        bytes: size,
                        last_t: now,
                        last_bytes: size,
                        speed: 0.0,
                        last_change: now,
                    },
                );
                vec![Update::Started(base.to_string()), Update::Changed]
            }
            Some(s) => {
                let mut changed = size != s.bytes;
                if size != s.bytes {
                    s.last_change = now;
                }
                s.bytes = size;
                let dt = now - s.last_t;
                if dt >= SPEED_WINDOW {
                    let inst = size.saturating_sub(s.last_bytes) as f64 / dt;
                    let before = s.speed;
                    s.speed = if now - s.last_change >= STALL_AFTER {
                        0.0
                    } else if s.speed == 0.0 {
                        inst
                    } else {
                        s.speed + ALPHA * (inst - s.speed)
                    };
                    changed |= s.speed != before;
                    s.last_t = now;
                    s.last_bytes = size;
                }
                if changed {
                    vec![Update::Changed]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// A file was renamed from `from` to `to`.
    pub fn renamed(&mut self, from: &str, to: &str, now: f64) -> Vec<Update> {
        let Some(state) = self.active.remove(from) else {
            // Not one of ours (or the first sighting was this rename): a rename *to* a partial name
            // starts tracking it.
            return if partial_base(to).is_some() {
                self.partial_seen(to, 0, now)
            } else {
                Vec::new()
            };
        };
        if partial_base(to).is_some() {
            // Browsers rename "Unconfirmed 123.crdownload" to the real name once they know it.
            let base = partial_base(to).unwrap_or("").to_string();
            self.active.insert(to.to_string(), State { base, ..state });
            vec![Update::Changed]
        } else if to == state.base {
            vec![
                Update::Finished {
                    name: state.base,
                    bytes: state.bytes,
                },
                Update::Changed,
            ]
        } else {
            vec![Update::Cancelled(state.base), Update::Changed]
        }
    }

    /// A file disappeared.
    pub fn removed(&mut self, name: &str) -> Vec<Update> {
        match self.active.remove(name) {
            Some(s) => vec![Update::Cancelled(s.base), Update::Changed],
            None => Vec::new(),
        }
    }

    /// Apply a batch of decoded directory changes. `size_of` reads a file's current size (it is only
    /// asked about partial files, and about the final file when a download completes, so the
    /// "Finished" size is the real one).
    pub fn apply(
        &mut self,
        changes: &[Change],
        mut size_of: impl FnMut(&str) -> Option<u64>,
        now: f64,
    ) -> Vec<Update> {
        let mut out = Vec::new();
        for change in changes {
            // A rename "from" is always followed at once by its "to"; anything else in between means
            // the file left the folder.
            if !matches!(change, Change::RenamedFrom(_) | Change::RenamedTo(_))
                && let Some(prev) = self.rename_from.take()
            {
                out.extend(self.removed(&prev));
            }
            match change {
                Change::Added(n) | Change::Modified(n) => {
                    if partial_base(n).is_some()
                        && let Some(size) = size_of(n)
                    {
                        out.extend(self.partial_seen(n, size, now));
                    }
                }
                Change::Removed(n) => out.extend(self.removed(n)),
                Change::RenamedFrom(n) => {
                    // An unpaired earlier "from" means the file left this folder: it is gone.
                    if let Some(prev) = self.rename_from.replace(n.clone()) {
                        out.extend(self.removed(&prev));
                    }
                }
                Change::RenamedTo(to) => match self.rename_from.take() {
                    Some(from) => out.extend(self.renamed(&from, to, now)),
                    None => {
                        if partial_base(to).is_some() {
                            let size = size_of(to).unwrap_or(0);
                            out.extend(self.partial_seen(to, size, now));
                        }
                    }
                },
            }
        }
        for u in &mut out {
            if let Update::Finished { name, bytes } = u
                && let Some(real) = size_of(name)
            {
                *bytes = real;
            }
        }
        out
    }

    /// Stop listing partial files that have not grown for `ABANDON_AFTER` seconds.
    pub fn expire(&mut self, now: f64) -> Vec<Update> {
        let before = self.active.len();
        self.active
            .retain(|_, s| now - s.last_change < ABANDON_AFTER);
        if self.active.len() != before {
            vec![Update::Changed]
        } else {
            Vec::new()
        }
    }

    /// The partial files being tracked (for the periodic size check).
    pub fn partial_names(&self) -> Vec<String> {
        self.active.keys().cloned().collect()
    }

    pub fn is_idle(&self) -> bool {
        self.active.is_empty()
    }

    /// The downloads in progress, largest first.
    pub fn active(&self) -> Vec<Active> {
        let mut v: Vec<Active> = self
            .active
            .values()
            .map(|s| Active {
                name: s.base.clone(),
                bytes: s.bytes,
                speed_bps: s.speed,
            })
            .collect();
        v.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        v
    }
}

/// "812 B", "4.2 KB", "12 MB", "1.3 GB".
pub fn fmt_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let x = b as f64;
    if x < K {
        format!("{b} B")
    } else if x < K * K {
        format!("{:.1} KB", x / K)
    } else if x < K * K * K {
        let v = x / (K * K);
        if v < 10.0 {
            format!("{v:.1} MB")
        } else {
            format!("{v:.0} MB")
        }
    } else {
        format!("{:.1} GB", x / (K * K * K))
    }
}

pub fn fmt_speed(bps: f64) -> String {
    format!("{}/s", fmt_bytes(bps.max(0.0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_files_are_recognised_by_extension() {
        assert_eq!(partial_base("foo.zip.crdownload"), Some("foo.zip"));
        assert_eq!(partial_base("Report (1).pdf.PART"), Some("Report (1).pdf"));
        assert_eq!(partial_base("x.exe.opdownload"), Some("x.exe"));
        assert_eq!(partial_base("foo.zip"), None);
        assert_eq!(partial_base(".crdownload"), None, "no base name");
        assert_eq!(partial_base("noextension"), None);
        assert_eq!(
            partial_base("archive.partial.txt"),
            None,
            "only the last extension counts"
        );
    }

    #[test]
    fn a_download_starts_grows_and_finishes_on_rename() {
        let mut t = Tracker::default();
        let u = t.partial_seen("a.zip.crdownload", 0, 0.0);
        assert!(u.contains(&Update::Started("a.zip".into())));
        assert_eq!(
            t.partial_seen("a.zip.crdownload", 1000, 0.1),
            vec![Update::Changed]
        );
        let u = t.partial_seen("a.zip.crdownload", 2_000_000, 1.0);
        assert_eq!(u, vec![Update::Changed]);
        let act = t.active();
        assert_eq!((act[0].name.as_str(), act[0].bytes), ("a.zip", 2_000_000));
        assert!(act[0].speed_bps > 1_000_000.0, "{}", act[0].speed_bps);
        let u = t.renamed("a.zip.crdownload", "a.zip", 1.2);
        assert!(u.contains(&Update::Finished {
            name: "a.zip".into(),
            bytes: 2_000_000
        }));
        assert!(t.is_idle());
    }

    #[test]
    fn speed_is_smoothed_and_samples_closer_than_the_window_are_not_averaged() {
        let mut t = Tracker::default();
        t.partial_seen("b.bin.part", 0, 0.0);
        t.partial_seen("b.bin.part", 1_000_000, 1.0); // 1 MB/s
        let first = t.active()[0].speed_bps;
        assert!((first - 1_000_000.0).abs() < 1.0);
        // A burst 0.1 s later does not spike the speed (too soon to measure).
        t.partial_seen("b.bin.part", 5_000_000, 1.1);
        assert!((t.active()[0].speed_bps - first).abs() < 1.0);
        // The next full sample moves it only part of the way.
        t.partial_seen("b.bin.part", 6_000_000, 2.0);
        let s = t.active()[0].speed_bps;
        assert!(s > first && s < 5_000_000.0, "{s}");
    }

    #[test]
    fn browsers_rename_the_partial_file_once_they_know_the_name() {
        let mut t = Tracker::default();
        t.partial_seen("Unconfirmed 12345.crdownload", 500, 0.0);
        let u = t.renamed("Unconfirmed 12345.crdownload", "movie.mp4.crdownload", 0.5);
        assert_eq!(u, vec![Update::Changed]);
        assert_eq!(t.active()[0].name, "movie.mp4");
        let u = t.renamed("movie.mp4.crdownload", "movie.mp4", 0.9);
        assert!(matches!(&u[0], Update::Finished { name, .. } if name == "movie.mp4"));
    }

    #[test]
    fn a_vanished_partial_file_is_a_cancelled_download() {
        let mut t = Tracker::default();
        t.partial_seen("c.iso.crdownload", 10, 0.0);
        let u = t.removed("c.iso.crdownload");
        assert_eq!(u[0], Update::Cancelled("c.iso".into()));
        assert!(t.is_idle());
        assert!(t.removed("c.iso.crdownload").is_empty());
        // Renamed to something unrelated: also not a completion.
        t.partial_seen("d.iso.crdownload", 10, 0.0);
        let u = t.renamed("d.iso.crdownload", "other.txt", 1.0);
        assert_eq!(u[0], Update::Cancelled("d.iso".into()));
    }

    #[test]
    fn an_unrelated_file_is_ignored_and_a_rename_into_a_partial_name_is_picked_up() {
        let mut t = Tracker::default();
        assert!(t.partial_seen("notes.txt", 5, 0.0).is_empty());
        assert!(t.renamed("a.txt", "b.txt", 0.0).is_empty());
        let u = t.renamed("tmp123", "e.zip.crdownload", 0.0);
        assert!(u.contains(&Update::Started("e.zip".into())));
        assert_eq!(t.partial_names(), vec!["e.zip.crdownload".to_string()]);
    }

    #[test]
    fn several_downloads_are_listed_largest_first() {
        let mut t = Tracker::default();
        t.partial_seen("small.bin.part", 10, 0.0);
        t.partial_seen("big.bin.part", 9000, 0.0);
        let names: Vec<String> = t.active().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["big.bin", "small.bin"]);
    }

    #[test]
    fn sizes_and_speeds_read_naturally() {
        assert_eq!(fmt_bytes(812), "812 B");
        assert_eq!(fmt_bytes(4300), "4.2 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_bytes(120 * 1024 * 1024), "120 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        assert_eq!(fmt_speed(2.5 * 1024.0 * 1024.0), "2.5 MB/s");
        assert_eq!(fmt_speed(-3.0), "0 B/s");
    }
    /// What the kernel writes: one `FILE_NOTIFY_INFORMATION` record per entry.
    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = Vec::new();
        v.extend(next.to_le_bytes());
        v.extend(action.to_le_bytes());
        v.extend(((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            v.extend(u.to_le_bytes());
        }
        v
    }

    fn chain(entries: &[(u32, &str)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for (i, (action, name)) in entries.iter().enumerate() {
            let mut r = record(0, *action, name);
            while !r.len().is_multiple_of(4) {
                r.push(0);
            }
            let next = if i + 1 < entries.len() {
                r.len() as u32
            } else {
                0
            };
            r[..4].copy_from_slice(&next.to_le_bytes());
            out.extend(r);
        }
        out
    }

    #[test]
    fn change_records_are_decoded_in_order() {
        let buf = chain(&[
            (1, "a.zip.crdownload"),
            (3, "a.zip.crdownload"),
            (4, "a.zip.crdownload"),
            (5, "a.zip"),
            (2, "gone ✓.txt"),
        ]);
        assert_eq!(
            parse_changes(&buf),
            vec![
                Change::Added("a.zip.crdownload".into()),
                Change::Modified("a.zip.crdownload".into()),
                Change::RenamedFrom("a.zip.crdownload".into()),
                Change::RenamedTo("a.zip".into()),
                Change::Removed("gone ✓.txt".into()),
            ]
        );
        assert!(parse_changes(&[]).is_empty());
    }

    #[test]
    fn malformed_change_buffers_never_panic_and_keep_what_was_intact() {
        let good = chain(&[(1, "x.part"), (2, "y.part")]);
        // Truncated anywhere: only whole records come out.
        for cut in 0..good.len() {
            let got = parse_changes(&good[..cut]);
            assert!(got.len() <= 2, "cut at {cut}: {got:?}");
        }
        assert_eq!(parse_changes(&good[..good.len() - 2]).len(), 1);
        // A name length that points past the end.
        let mut bad = record(0, 1, "z");
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_changes(&bad).is_empty());
        // A next-offset that points nowhere, or loops back on itself harmlessly forward.
        let mut jump = record(u32::MAX, 1, "z");
        jump.extend([0u8; 16]);
        assert_eq!(parse_changes(&jump).len(), 1);
        // Unknown actions are skipped, odd name lengths are tolerated.
        assert!(parse_changes(&record(0, 99, "q")).is_empty());
        let mut odd = record(0, 1, "ab");
        odd[8..12].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(parse_changes(&odd), vec![Change::Added("a".into())]);
        // Pseudo-random garbage of every length.
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for len in 0..200usize {
            let junk: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect();
            let _ = parse_changes(&junk);
        }
    }

    #[test]
    fn only_plain_absolute_paths_may_be_revealed() {
        for ok in [
            r"C:\Users\me\Downloads\a.zip",
            r"d:\x\y",
            r"\\nas\share\file.bin",
        ] {
            assert!(is_plain_absolute_path(ok), "{ok}");
        }
        for bad in [
            "",
            "a.zip",
            r"Downloads\a.zip",
            r"C:a.zip",
            r"C:\",
            "https://example.com/a.zip",
            r"\\?\C:\a.zip",
            r"\\.\pipe\x",
            r"\\server",
            r"\\\share\x",
            "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
            "C:\\a\u{0}b",
            "C:\\a\nb",
        ] {
            assert!(!is_plain_absolute_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn display_names_hide_what_could_mislead() {
        assert_eq!(display_name("report.pdf"), "report.pdf");
        assert_eq!(
            display_name("invoice\u{202E}fdp.exe"),
            "invoicefdp.exe",
            "no right-to-left override"
        );
        assert_eq!(display_name("a\u{7}b\nc.txt"), "abc.txt");
        let long = format!("{}.iso", "x".repeat(300));
        let shown = display_name(&long);
        assert!(shown.chars().count() <= 80, "{}", shown.chars().count());
        assert!(shown.ends_with(".iso") && shown.starts_with('…'));
    }

    #[test]
    fn a_whole_download_through_the_change_stream() {
        let mut t = Tracker::default();
        let size = |n: &str| match n {
            "movie.mp4.crdownload" => Some(4096),
            "movie.mp4" => Some(8_000_000),
            _ => None,
        };
        let u = t.apply(
            &[Change::Added("Unconfirmed 1.crdownload".into())],
            |_| Some(0),
            0.0,
        );
        assert!(u.contains(&Update::Started("Unconfirmed 1".into())));
        // Chrome learns the real name and renames the partial file.
        let u = t.apply(
            &[
                Change::RenamedFrom("Unconfirmed 1.crdownload".into()),
                Change::RenamedTo("movie.mp4.crdownload".into()),
                Change::Modified("movie.mp4.crdownload".into()),
            ],
            size,
            0.5,
        );
        assert!(!u.is_empty());
        assert_eq!(t.active()[0].name, "movie.mp4");
        assert_eq!(t.active()[0].bytes, 4096);
        // Completion: the real size comes from the finished file, not the last sample.
        let u = t.apply(
            &[
                Change::RenamedFrom("movie.mp4.crdownload".into()),
                Change::RenamedTo("movie.mp4".into()),
            ],
            size,
            2.0,
        );
        assert!(u.contains(&Update::Finished {
            name: "movie.mp4".into(),
            bytes: 8_000_000
        }));
        assert!(t.is_idle());
    }

    #[test]
    fn a_rename_pair_may_straddle_two_reads_and_a_stray_from_means_the_file_left() {
        let mut t = Tracker::default();
        t.partial_seen("a.bin.part", 10, 0.0);
        assert!(
            t.apply(&[Change::RenamedFrom("a.bin.part".into())], |_| None, 1.0)
                .is_empty()
        );
        let u = t.apply(&[Change::RenamedTo("a.bin".into())], |_| Some(99), 1.1);
        assert!(u.contains(&Update::Finished {
            name: "a.bin".into(),
            bytes: 99
        }));

        t.partial_seen("b.bin.part", 10, 0.0);
        t.apply(&[Change::RenamedFrom("b.bin.part".into())], |_| None, 1.0);
        let u = t.apply(&[Change::Added("other.txt".into())], |_| Some(1), 1.1);
        assert!(u.contains(&Update::Cancelled("b.bin".into())), "{u:?}");
        assert!(t.is_idle());
    }

    #[test]
    fn a_download_that_stops_growing_loses_its_speed_and_is_eventually_dropped() {
        let mut t = Tracker::default();
        t.partial_seen("s.iso.crdownload", 0, 0.0);
        t.partial_seen("s.iso.crdownload", 3_000_000, 1.0);
        assert!(t.active()[0].speed_bps > 1_000_000.0);
        // Nothing new for a while: the speed reads zero (and that is reported once).
        t.partial_seen("s.iso.crdownload", 3_000_000, 2.0);
        assert!(
            t.active()[0].speed_bps > 0.0,
            "one quiet second is not a stall yet"
        );
        let u = t.partial_seen("s.iso.crdownload", 3_000_000, 4.0);
        assert_eq!(u, vec![Update::Changed]);
        assert_eq!(t.active()[0].speed_bps, 0.0);
        assert!(
            t.partial_seen("s.iso.crdownload", 3_000_000, 5.0)
                .is_empty(),
            "quiet stays quiet"
        );
        // Still listed after a few minutes (a paused download), gone after ABANDON_AFTER.
        assert!(t.expire(300.0).is_empty());
        assert_eq!(t.expire(ABANDON_AFTER + 4.1), vec![Update::Changed]);
        assert!(t.is_idle());
        assert!(t.expire(10_000.0).is_empty());
    }
}
