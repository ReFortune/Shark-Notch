//! Browser downloads in progress, seen from the file system alone.
//!
//! A browser writes a download into a partial file (`name.zip.crdownload`, `.part`, ...) in the
//! Downloads folder and renames it when it is complete. This service asks Windows to tell it when
//! something in that folder changes (`ReadDirectoryChangesW`, overlapped: the thread sleeps until
//! there is news) and keeps a [`Tracker`] up to date. NTFS reports size changes lazily, so while a
//! partial file exists the thread also looks at its size once a second; with no partial file it is
//! asleep with no timer at all.
//!
//! Nothing here reads a file's contents, opens or runs a download, or talks to a browser: only
//! names and sizes. The final size of a download is not knowable from the file system, so there is
//! no percentage; the module shows the size so far and the speed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use notch_core::bus::BusSender;
use notch_core::downloads::{Change, Tracker, Update, display_name, parse_changes, partial_base};
use notch_core::events::{ActiveDownload, DownloadDone, EventKind, Source};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_INCOMPLETE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{INFINITE, SetEvent, WaitForMultipleObjects};
use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::PCWSTR;

use crate::win::util::{EventHandle, wide};

/// How often the sizes of partial files are looked at while any exist.
const POLL: Duration = Duration::from_secs(1);
/// The list sent to the UI changes at most this often (a download that appears or ends is sent at
/// once).
const PUBLISH_GAP: Duration = Duration::from_secs(1);
/// A partial file untouched for this long is not a download in progress when the app starts.
const RECENT: Duration = Duration::from_secs(120);
/// Downloads listed on the page and pill.
const SHOWN: usize = 8;
/// `FILE_NOTIFY_INFORMATION` buffer, in 32-bit words (it must be DWORD-aligned). When it overflows
/// the folder is simply looked at again.
const BUF_WORDS: usize = 4096;

pub struct DownloadsService {
    /// The `live.download_dir` value this was started with ("" = the Downloads folder).
    setting: String,
    quit: Arc<EventHandle>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

/// `C:\Users\you\Downloads`, wherever the user has moved it to.
fn known_downloads() -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

/// The folder to watch for a `download_dir` setting, if it names a usable one.
fn resolve_dir(setting: &str) -> Option<PathBuf> {
    let setting = setting.trim();
    let dir = if setting.is_empty() {
        known_downloads()?
    } else {
        PathBuf::from(setting)
    };
    if dir.is_absolute() && dir.is_dir() {
        Some(dir)
    } else {
        crate::warn!(
            "downloads: \"{}\" is not a folder that can be watched",
            dir.display()
        );
        None
    }
}

impl DownloadsService {
    pub fn start(setting: &str, bus: BusSender) -> Option<DownloadsService> {
        let dir = resolve_dir(setting)?;
        let quit = Arc::new(EventHandle::new(true)?);
        let done = Arc::new(AtomicBool::new(false));
        let (q2, d2) = (quit.clone(), done.clone());
        let thread = std::thread::Builder::new()
            .name("downloads".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(&dir, &bus, &q2);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the downloads watcher: {e}"))
            .ok()?;
        Some(DownloadsService {
            setting: setting.to_string(),
            quit,
            thread: Some(thread),
            done,
        })
    }

    pub fn setting(&self) -> &str {
        &self.setting
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

/// What came back from a directory read.
enum Read {
    Changes(Vec<u8>),
    /// The kernel's buffer overflowed: some changes were lost.
    Overflow,
    /// Not finished after all (a spurious wake).
    Pending,
    Failed,
}

/// An open directory with one overlapped `ReadDirectoryChangesW` outstanding at a time.
struct DirWatch {
    dir: HANDLE,
    /// Signalled by the kernel when the outstanding read completes.
    event: EventHandle,
    // Boxed: the kernel holds these addresses while a read is pending, so they must not move.
    ov: Box<OVERLAPPED>,
    buf: Box<[u32; BUF_WORDS]>,
    pending: bool,
}

impl DirWatch {
    fn open(dir: &Path) -> Option<DirWatch> {
        let w = wide(&dir.to_string_lossy());
        let handle = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|e| crate::warn!("downloads: cannot open the folder ({e})"))
        .ok()?;
        let Some(event) = EventHandle::new(true) else {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        };
        let ov = Box::new(OVERLAPPED {
            hEvent: event.0,
            ..OVERLAPPED::default()
        });
        Some(DirWatch {
            dir: handle,
            event,
            ov,
            buf: Box::new([0; BUF_WORDS]),
            pending: false,
        })
    }

    /// Make sure a read is outstanding.
    fn arm(&mut self) -> bool {
        if self.pending {
            return true;
        }
        let rc = unsafe {
            ReadDirectoryChangesW(
                self.dir,
                self.buf.as_mut_ptr().cast(),
                (BUF_WORDS * 4) as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE,
                None,
                Some(&raw mut *self.ov),
                None,
            )
        };
        match rc {
            Ok(()) => {
                self.pending = true;
                true
            }
            Err(e) => {
                crate::warn!("downloads: cannot watch the folder ({e})");
                false
            }
        }
    }

    /// Collect the result of the outstanding read, after its event was signalled.
    fn take(&mut self) -> Read {
        let mut n = 0u32;
        match unsafe { GetOverlappedResult(self.dir, &*self.ov, &mut n, false) } {
            Ok(()) => {
                self.pending = false;
                if n == 0 {
                    return Read::Overflow;
                }
                let n = (n as usize).min(BUF_WORDS * 4);
                // The kernel filled `n` bytes of the buffer.
                let bytes: Vec<u8> = self
                    .buf
                    .iter()
                    .flat_map(|w| w.to_le_bytes())
                    .take(n)
                    .collect();
                Read::Changes(bytes)
            }
            Err(e) if e.code() == ERROR_IO_INCOMPLETE.to_hresult() => Read::Pending,
            Err(e) => {
                self.pending = false;
                crate::warn!("downloads: the folder watch failed ({e})");
                Read::Failed
            }
        }
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        unsafe {
            if self.pending {
                // The kernel still owns the buffer and the OVERLAPPED until the read has completed.
                let _ = CancelIoEx(self.dir, Some(&raw const *self.ov));
                let mut n = 0u32;
                let _ = GetOverlappedResult(self.dir, &*self.ov, &mut n, true);
            }
            let _ = CloseHandle(self.dir);
        }
    }
}

/// Tell the UI about the downloads, without flooding it.
struct Publisher<'a> {
    bus: &'a BusSender,
    dir: &'a Path,
    last: Vec<ActiveDownload>,
    last_at: Instant,
    /// The list differs from what the UI has, but it was sent too recently to send again yet.
    dirty: bool,
}

impl Publisher<'_> {
    fn current(tracker: &Tracker) -> Vec<ActiveDownload> {
        tracker
            .active()
            .into_iter()
            .take(SHOWN)
            .map(|a| ActiveDownload {
                name: display_name(&a.name).into(),
                bytes: a.bytes,
                speed_bps: a.speed_bps.max(0.0) as u64,
            })
            .collect()
    }

    /// Send the list if it changed: at once when a download appeared or went away, otherwise at
    /// most once per `PUBLISH_GAP`.
    fn flush(&mut self, tracker: &Tracker) {
        let now_list = Self::current(tracker);
        if now_list == self.last {
            self.dirty = false;
            return;
        }
        let same_names = now_list.len() == self.last.len()
            && now_list
                .iter()
                .zip(&self.last)
                .all(|(a, b)| a.name == b.name);
        if same_names && self.last_at.elapsed() < PUBLISH_GAP {
            self.dirty = true;
            return;
        }
        self.last = now_list.clone();
        self.last_at = Instant::now();
        self.dirty = false;
        self.bus
            .send(Source::Local, EventKind::Downloads(Arc::new(now_list)));
    }

    /// When a held-back list is due.
    fn due(&self) -> Option<Instant> {
        self.dirty.then(|| self.last_at + PUBLISH_GAP)
    }

    fn finished(&self, name: &str, bytes: u64) {
        crate::debug!("downloads: a download finished ({bytes} bytes)");
        let path = self.dir.join(name);
        self.bus.send(
            Source::Local,
            EventKind::DownloadDone(DownloadDone {
                name: display_name(name).into(),
                path: Arc::from(&*path.to_string_lossy()),
                bytes,
            }),
        );
    }
}

/// Size of a file in the folder, if it can be read.
fn size_in(dir: &Path, name: &str) -> Option<u64> {
    std::fs::metadata(dir.join(name)).ok().map(|m| m.len())
}

/// Partial files that were written to a moment ago (the downloads in progress when the app started,
/// or after the kernel dropped some changes).
fn scan_recent(dir: &Path, tracker: &mut Tracker, now: f64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut seen = 0;
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if partial_base(&name).is_none() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let recent = meta
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_none_or(|age| age < RECENT);
        if recent && meta.is_file() {
            tracker.partial_seen(&name, meta.len(), now);
            seen += 1;
            if seen >= 64 {
                break;
            }
        }
    }
}

/// Look at the size of every partial file being tracked.
fn poll_sizes(dir: &Path, tracker: &mut Tracker, now: f64) -> Vec<Update> {
    let mut out = Vec::new();
    for name in tracker.partial_names() {
        match std::fs::metadata(dir.join(&name)) {
            Ok(m) => out.extend(tracker.partial_seen(&name, m.len(), now)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                out.extend(tracker.removed(&name))
            }
            Err(_) => {}
        }
    }
    out.extend(tracker.expire(now));
    out
}

fn run(dir: &Path, bus: &BusSender, quit: &EventHandle) {
    let Some(mut watch) = DirWatch::open(dir) else {
        return;
    };
    let epoch = Instant::now();
    let secs = |at: Instant| at.duration_since(epoch).as_secs_f64();
    let mut tracker = Tracker::default();
    let mut publisher = Publisher {
        bus,
        dir,
        last: Vec::new(),
        last_at: epoch,
        dirty: false,
    };
    // Arm first, then look: a change in between is then caught by the watch.
    if !watch.arm() {
        return;
    }
    scan_recent(dir, &mut tracker, 0.0);
    publisher.flush(&tracker);
    let mut last_poll = epoch;

    let handle = |updates: Vec<Update>, publisher: &Publisher| {
        for u in updates {
            if let Update::Finished { name, bytes } = u {
                publisher.finished(&name, bytes);
            }
        }
    };

    loop {
        if !watch.arm() {
            break;
        }
        // Sleep until the folder changes, or — only while a download is being written — until the
        // next size check or the next held-back update is due.
        let mut deadline: Option<Instant> = publisher.due();
        if !tracker.is_idle() {
            let poll_at = last_poll + POLL;
            deadline = Some(deadline.map_or(poll_at, |d| d.min(poll_at)));
        }
        let wait_ms = deadline.map_or(INFINITE, |d| {
            d.saturating_duration_since(Instant::now())
                .as_millis()
                .min(u128::from(INFINITE - 1)) as u32
        });
        let woke = unsafe { WaitForMultipleObjects(&[watch.event.0, quit.0], false, wait_ms) };
        if woke.0 == WAIT_OBJECT_0.0 + 1 {
            break;
        }
        let now = Instant::now();
        if woke == WAIT_OBJECT_0 {
            match watch.take() {
                Read::Changes(bytes) => {
                    let changes: Vec<Change> = parse_changes(&bytes);
                    let updates = tracker.apply(&changes, |n| size_in(dir, n), secs(now));
                    handle(updates, &publisher);
                }
                Read::Overflow => scan_recent(dir, &mut tracker, secs(now)),
                Read::Pending => {}
                Read::Failed => break,
            }
        } else if woke != WAIT_TIMEOUT {
            break;
        }
        if !tracker.is_idle() && now >= last_poll + POLL {
            last_poll = now;
            let updates = poll_sizes(dir, &mut tracker, secs(now));
            handle(updates, &publisher);
        }
        publisher.flush(&tracker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notch_core::bus::{Bus, Waker};

    struct NoWake;
    impl Waker for NoWake {
        fn wake(&self) {}
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shark-notch-dl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn collect_until(bus: &mut Bus, secs: u64, mut done: impl FnMut(&EventKind) -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(secs);
        let mut got = Vec::new();
        while Instant::now() < until {
            bus.drain(&mut got);
            for ev in got.drain(..) {
                if done(&ev.kind) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_partial_file_growing_and_being_renamed_is_a_download() {
        let dir = scratch("flow");
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = DownloadsService::start(&dir.to_string_lossy(), tx).expect("the watcher starts");

        let partial = dir.join("movie.mp4.crdownload");
        std::fs::write(&partial, vec![7u8; 100_000]).unwrap();
        let seen = collect_until(
            &mut bus,
            8,
            |k| matches!(k, EventKind::Downloads(d) if d.iter().any(|a| &*a.name == "movie.mp4")),
        );
        assert!(seen, "the download appears in the list");

        // It grows: a later list carries the new size.
        std::fs::write(&partial, vec![7u8; 400_000]).unwrap();
        let grew = collect_until(
            &mut bus,
            8,
            |k| matches!(k, EventKind::Downloads(d) if d.iter().any(|a| a.bytes == 400_000)),
        );
        assert!(grew, "the size is followed");

        // Completion: renamed to the real name.
        std::fs::rename(&partial, dir.join("movie.mp4")).unwrap();
        let mut finished = None;
        let mut emptied = false;
        let until = Instant::now() + Duration::from_secs(8);
        let mut got = Vec::new();
        while Instant::now() < until && !(finished.is_some() && emptied) {
            bus.drain(&mut got);
            for ev in got.drain(..) {
                match ev.kind {
                    EventKind::DownloadDone(d) => finished = Some(d),
                    EventKind::Downloads(d) if d.is_empty() => emptied = true,
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let done = finished.expect("a completion is announced");
        assert_eq!(&*done.name, "movie.mp4");
        assert_eq!(done.bytes, 400_000, "the size of the finished file");
        assert!(done.path.ends_with("movie.mp4"), "{}", done.path);
        assert!(emptied, "and the list empties");

        svc.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ordinary_files_and_a_cancelled_download_are_not_completions() {
        let dir = scratch("cancel");
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = DownloadsService::start(&dir.to_string_lossy(), tx).expect("the watcher starts");
        std::fs::write(dir.join("notes.txt"), b"hello").unwrap();
        let partial = dir.join("big.iso.part");
        std::fs::write(&partial, b"x").unwrap();
        assert!(collect_until(&mut bus, 8, |k| {
            matches!(k, EventKind::Downloads(d) if !d.is_empty())
        }));
        std::fs::remove_file(&partial).unwrap();
        let mut done_seen = false;
        let gone = collect_until(&mut bus, 8, |k| {
            if matches!(k, EventKind::DownloadDone(_)) {
                done_seen = true;
            }
            matches!(k, EventKind::Downloads(d) if d.is_empty())
        });
        assert!(gone && !done_seen);
        svc.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_already_in_progress_when_the_app_starts_is_listed() {
        let dir = scratch("startup");
        std::fs::write(dir.join("old.bin.crdownload"), vec![1u8; 10]).unwrap();
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = DownloadsService::start(&dir.to_string_lossy(), tx).expect("the watcher starts");
        assert!(collect_until(&mut bus, 8, |k| {
            matches!(k, EventKind::Downloads(d) if d.iter().any(|a| &*a.name == "old.bin"))
        }));
        svc.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_does_not_exist_is_refused_and_a_relative_one_too() {
        let (_bus, tx) = Bus::new(Arc::new(NoWake));
        assert!(DownloadsService::start(r"C:\definitely\not\here\at-all", tx.clone()).is_none());
        assert!(DownloadsService::start(r"Downloads", tx).is_none());
    }

    #[test]
    fn stopping_returns_promptly_even_while_idle() {
        let dir = scratch("stop");
        let (_bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = DownloadsService::start(&dir.to_string_lossy(), tx).expect("starts");
        let t = Instant::now();
        svc.stop();
        assert!(
            t.elapsed() < Duration::from_millis(600),
            "{:?}",
            t.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_known_downloads_folder_resolves() {
        let p = known_downloads().expect("the Downloads folder is known");
        assert!(p.is_absolute());
    }
}
