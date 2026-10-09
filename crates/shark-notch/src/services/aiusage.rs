//! Claude Code's token use today, from the session logs it keeps under `~/.claude/projects`.
//!
//! The thread sleeps until Windows says something under that folder changed
//! (`FindFirstChangeNotificationW`, whole tree), waits a moment for the burst to end, reads only
//! the bytes each log has grown by since last time, and sends one `AiUsage` event with the totals
//! since local midnight. It wakes by itself once a day, at midnight, so the totals reset. Nothing
//! is sent anywhere and no conversation text is kept (see `notch_core::aiusage`).
//!
//! NTFS reports the growth of a file that is still open for writing lazily, so the figures can lag
//! the conversation by a few seconds.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notch_core::aiusage::{Ledger, Totals};
use notch_core::bus::BusSender;
use notch_core::events::{EventKind, Source};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FindFirstChangeNotificationW, FindNextChangeNotification,
};
use windows::Win32::System::Threading::{INFINITE, SetEvent, WaitForMultipleObjects};

use crate::win::util::{EventHandle, pcwstr, wide};

/// How long a burst of writes may go on before the logs are read.
const SETTLE: Duration = Duration::from_millis(700);
/// How deep under `projects` the logs are looked for (`project/session.jsonl`, and the sub-agents'
/// logs further down).
const DEPTH: usize = 4;

pub struct AiUsageService {
    quit: Arc<EventHandle>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

/// `%CLAUDE_CONFIG_DIR%\projects`, else `%USERPROFILE%\.claude\projects`, if it exists.
fn projects_dir() -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".claude")))?;
    let dir = base.join("projects");
    dir.is_dir().then_some(dir)
}

impl AiUsageService {
    pub fn start(bus: BusSender) -> Option<AiUsageService> {
        let Some(dir) = projects_dir() else {
            crate::debug!("ai usage: no Claude Code logs folder on this PC");
            return None;
        };
        let quit = Arc::new(EventHandle::new(true)?);
        let done = Arc::new(AtomicBool::new(false));
        let (q2, d2) = (quit.clone(), done.clone());
        let thread = std::thread::Builder::new()
            .name("ai-usage".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(&dir, &bus, &q2);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the AI usage reader: {e}"))
            .ok()?;
        Some(AiUsageService {
            quit,
            thread: Some(thread),
            done,
        })
    }

    pub fn stop(mut self) {
        unsafe {
            let _ = SetEvent(self.quit.0);
        }
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(500);
            while !self.done.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.done.load(Ordering::Acquire) {
                let _ = h.join();
            }
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Unix time of the last local midnight, and the seconds until the next one.
fn local_midnight(now: i64) -> (i64, u64) {
    let t = crate::win::sys::local_time();
    let into_day = i64::from(t.hour) * 3600 + i64::from(t.minute) * 60 + i64::from(t.second);
    (now - into_day, (86_400 - into_day).max(1) as u64)
}

/// Every `*.jsonl` under `dir`, a few levels down.
fn logs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() && depth > 1 => logs(&p, depth - 1, out),
            Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
            _ => {}
        }
    }
}

/// What has been read of each log so far (bytes), and everything seen.
#[derive(Default)]
struct Reader {
    offsets: HashMap<PathBuf, u64>,
    ledger: Ledger,
}

impl Reader {
    /// Read what the logs grew by. A log not written since `since` (Unix seconds) holds nothing
    /// from today, so it is not opened.
    fn scan(&mut self, dir: &Path, since: i64) {
        let mut files = Vec::new();
        logs(dir, DEPTH, &mut files);
        for f in files {
            let Ok(meta) = std::fs::metadata(&f) else {
                continue;
            };
            let modified = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            let seen = self.offsets.get(&f).copied();
            if seen.is_none() && modified < since {
                continue;
            }
            let from = match seen {
                Some(o) if o <= meta.len() => o,
                _ => 0, // new, or shorter than before: start again
            };
            if from == meta.len() {
                continue;
            }
            if let Some(read) = self.read_from(&f, from) {
                self.offsets.insert(f, from + read);
            }
        }
    }

    /// Feed the ledger the complete lines from `from` on; returns how many bytes were consumed.
    fn read_from(&mut self, path: &Path, from: u64) -> Option<u64> {
        let mut file = std::fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(from)).ok()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        // A line still being written is left for next time.
        let end = bytes.iter().rposition(|&b| b == b'\n')? + 1;
        self.ledger
            .add_text(&String::from_utf8_lossy(&bytes[..end]));
        Some(end as u64)
    }
}

fn run(dir: &Path, bus: &BusSender, quit: &EventHandle) {
    let d = wide(&dir.to_string_lossy());
    let watch = unsafe {
        FindFirstChangeNotificationW(
            pcwstr(&d),
            true,
            FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_SIZE,
        )
    };
    let Ok(watch) = watch else {
        crate::warn!("ai usage: cannot watch the Claude Code logs folder");
        return;
    };
    let mut reader = Reader::default();
    let mut last: Option<Totals> = None;
    loop {
        let now = unix_now();
        let (midnight, secs_left) = local_midnight(now);
        reader.scan(dir, midnight);
        reader.ledger.prune(midnight);
        let totals = reader.ledger.totals_since(midnight);
        if last != Some(totals) {
            bus.send(Source::Local, EventKind::AiUsage(totals));
            last = Some(totals);
        }
        // Asleep until a log changes, or midnight (when "today" starts over).
        let ms = (secs_left * 1000).min(u64::from(INFINITE - 1)) as u32;
        let woke = unsafe { WaitForMultipleObjects(&[quit.0, watch], false, ms) };
        if woke == WAIT_OBJECT_0 {
            break;
        }
        if woke.0 == WAIT_OBJECT_0.0 + 1 {
            unsafe {
                let _ = FindNextChangeNotification(watch);
            }
            // Let the burst finish (or quit meanwhile).
            if unsafe { WaitForMultipleObjects(&[quit.0], false, SETTLE.as_millis() as u32) }
                == WAIT_OBJECT_0
            {
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(watch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, out: u64) -> String {
        format!(
            r#"{{"timestamp":"2026-10-09T03:25:21.000Z","message":{{"id":"{id}","usage":{{"input_tokens":1,"output_tokens":{out}}}}}}}"#
        )
    }

    #[test]
    fn only_what_a_log_grew_by_is_read_and_a_half_written_line_waits() {
        let dir = std::env::temp_dir().join(format!("sn-ai-{}", std::process::id()));
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let log = proj.join("s.jsonl");
        std::fs::write(&log, format!("{}\n{}", line("a", 5), &line("b", 7)[..20])).unwrap();
        let mut r = Reader::default();
        r.scan(&dir, 0);
        assert_eq!(r.ledger.totals_since(0).messages, 1, "the cut line waits");
        // The line is completed and another follows.
        std::fs::write(
            &log,
            format!("{}\n{}\n{}\n", line("a", 5), line("b", 7), line("c", 9)),
        )
        .unwrap();
        r.scan(&dir, 0);
        let t = r.ledger.totals_since(0);
        assert_eq!((t.messages, t.output), (3, 21));
        // Nothing new: nothing changes.
        r.scan(&dir, 0);
        assert_eq!(r.ledger.totals_since(0), t);
        // A log that was not touched today is never opened.
        let mut late = Reader::default();
        late.scan(&dir, i64::MAX);
        assert_eq!(late.ledger.totals_since(0).messages, 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
