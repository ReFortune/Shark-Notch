//! A tiny persistent store: one JSON document per key under the app's data folder
//! (`%LOCALAPPDATA%\SharkNotch\<key>.json`). Modules ask for a key to be loaded and hand over new
//! contents to save; neither ever blocks the UI thread.
//!
//! Saves are written a moment after the last change (so typing a task does not write a file per
//! keystroke), atomically (write a temporary file, then rename over the old one), and flushed when
//! the service stops. Loads answer with a `StoreLoaded` event (`data: None` if nothing was saved).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notch_core::bus::BusSender;
use notch_core::events::{EventKind, Source, StoreItem};
use notch_core::module::StoreCmd;

use crate::win::paths;

/// A save waits this long for further changes before it is written.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// Refuse to read or write more than this per document.
const MAX_BYTES: usize = 1 << 20;

enum Req {
    Cmd(StoreCmd),
    Quit,
}

pub struct StoreService {
    tx: Sender<Req>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

/// Keys name files: lower-case letters, digits, `_` and `-` only.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 40
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn file_for(dir: &std::path::Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

/// Write `data` to `path` atomically.
fn write_atomic(path: &std::path::Path, data: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

impl StoreService {
    pub fn start(bus: BusSender) -> Option<StoreService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let thread = std::thread::Builder::new()
            .name("store".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                run(rx, bus, paths::data_dir());
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the store worker: {e}"))
            .ok()?;
        Some(StoreService {
            tx,
            thread: Some(thread),
            done,
        })
    }

    pub fn command(&self, cmd: &StoreCmd) {
        let _ = self.tx.send(Req::Cmd(cmd.clone()));
    }

    /// Flush pending saves and stop (waits briefly: losing the last edit would be worse than a short
    /// pause at exit).
    pub fn stop(mut self) {
        let _ = self.tx.send(Req::Quit);
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(1500);
            while !self.done.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.done.load(Ordering::Acquire) {
                let _ = h.join();
            }
        }
    }
}

fn run(rx: Receiver<Req>, bus: BusSender, dir: PathBuf) {
    // key -> (data, when it may be written)
    let mut pending: HashMap<&'static str, (Arc<str>, Instant)> = HashMap::new();
    loop {
        let msg = match pending.values().map(|(_, t)| *t).min() {
            Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Req::Cmd(StoreCmd::Load(key))) => {
                let data = if valid_key(key) {
                    // A load must see what was saved a moment ago, not what is on disk.
                    pending.get(key).map(|(d, _)| d.clone()).or_else(|| {
                        let path = file_for(&dir, key);
                        let ok_size =
                            std::fs::metadata(&path).is_ok_and(|m| m.len() as usize <= MAX_BYTES);
                        ok_size
                            .then(|| std::fs::read_to_string(&path).ok())
                            .flatten()
                            .map(Arc::from)
                    })
                } else {
                    crate::warn!("store: refusing to load an invalid key");
                    None
                };
                bus.send(
                    Source::Local,
                    EventKind::StoreLoaded(StoreItem {
                        key: key.into(),
                        data,
                    }),
                );
            }
            Ok(Req::Cmd(StoreCmd::Save { key, data })) => {
                if valid_key(key) && data.len() <= MAX_BYTES {
                    pending.insert(key, (data, Instant::now() + DEBOUNCE));
                } else {
                    crate::warn!("store: refusing to save an invalid key or an oversized document");
                }
            }
            Ok(Req::Quit) | Err(RecvTimeoutError::Disconnected) => {
                flush(&mut pending, &dir, true);
                return;
            }
            Err(RecvTimeoutError::Timeout) => flush(&mut pending, &dir, false),
        }
    }
}

/// Write the saves that are due (all of them with `all`).
fn flush(
    pending: &mut HashMap<&'static str, (Arc<str>, Instant)>,
    dir: &std::path::Path,
    all: bool,
) {
    let now = Instant::now();
    let due: Vec<&'static str> = pending
        .iter()
        .filter(|(_, (_, t))| all || *t <= now)
        .map(|(k, _)| *k)
        .collect();
    for key in due {
        if let Some((data, _)) = pending.remove(key)
            && let Err(e) = write_atomic(&file_for(dir, key), &data)
        {
            crate::warn!("store: cannot save '{key}': {e}");
        }
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

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("shark-notch-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn drain_until(bus: &mut Bus, want: usize) -> Vec<notch_core::events::Event> {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while got.len() < want && Instant::now() < until {
            bus.drain(&mut got);
            std::thread::sleep(Duration::from_millis(5));
        }
        got
    }

    fn loaded(ev: &notch_core::events::Event) -> (String, Option<String>) {
        match &ev.kind {
            EventKind::StoreLoaded(i) => {
                (i.key.to_string(), i.data.as_ref().map(|d| d.to_string()))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn keys_are_restricted_to_safe_file_names() {
        assert!(valid_key("pomodoro") && valid_key("a-b_c9"));
        for bad in [
            "",
            "..",
            "a/b",
            "a\\b",
            "A",
            "a b",
            "a.json",
            &"x".repeat(41),
            "naïve",
        ] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn saving_loading_and_atomic_replacement() {
        let dir = scratch("roundtrip");
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let (stx, srx) = channel();
        let d2 = dir.clone();
        let h = std::thread::spawn(move || run(srx, tx, d2));

        stx.send(Req::Cmd(StoreCmd::Load("notes"))).unwrap();
        let ev = drain_until(&mut bus, 1);
        assert_eq!(loaded(&ev[0]), ("notes".into(), None), "nothing saved yet");

        stx.send(Req::Cmd(StoreCmd::Save {
            key: "notes",
            data: "{\"v\":1}".into(),
        }))
        .unwrap();
        // Before the debounce elapses a load still sees the new value.
        stx.send(Req::Cmd(StoreCmd::Load("notes"))).unwrap();
        let ev = drain_until(&mut bus, 1);
        assert_eq!(loaded(&ev[0]), ("notes".into(), Some("{\"v\":1}".into())));
        assert!(
            !file_for(&dir, "notes").exists(),
            "not written yet: the save is debounced"
        );

        // Rapid changes collapse into one write of the last value.
        for i in 2..6 {
            stx.send(Req::Cmd(StoreCmd::Save {
                key: "notes",
                data: format!("{{\"v\":{i}}}").into(),
            }))
            .unwrap();
        }
        let until = Instant::now() + Duration::from_secs(5);
        while !file_for(&dir, "notes").exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(file_for(&dir, "notes")).unwrap(),
            "{\"v\":5}"
        );
        assert!(
            !dir.join("notes.json.tmp").exists(),
            "no temporary file is left behind"
        );

        stx.send(Req::Cmd(StoreCmd::Load("notes"))).unwrap();
        let ev = drain_until(&mut bus, 1);
        assert_eq!(loaded(&ev[0]).1.as_deref(), Some("{\"v\":5}"));
        stx.send(Req::Quit).unwrap();
        h.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quitting_flushes_pending_saves_and_bad_requests_are_refused() {
        let dir = scratch("flush");
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let (stx, srx) = channel();
        let d2 = dir.clone();
        let h = std::thread::spawn(move || run(srx, tx, d2));
        stx.send(Req::Cmd(StoreCmd::Save {
            key: "late",
            data: "last words".into(),
        }))
        .unwrap();
        stx.send(Req::Cmd(StoreCmd::Save {
            key: "../evil",
            data: "x".into(),
        }))
        .unwrap();
        stx.send(Req::Cmd(StoreCmd::Load("../evil"))).unwrap();
        stx.send(Req::Quit).unwrap();
        h.join().unwrap();
        assert_eq!(
            std::fs::read_to_string(file_for(&dir, "late")).unwrap(),
            "last words"
        );
        assert!(!dir.join("../evil.json").exists());
        let ev = drain_until(&mut bus, 1);
        assert_eq!(
            loaded(&ev[0]),
            ("../evil".into(), None),
            "an invalid key loads as nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
