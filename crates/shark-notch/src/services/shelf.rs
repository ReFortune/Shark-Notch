//! The file shelf's platform side: turns "these paths were dropped on the notch" into
//! `FileDropped` events with names, sizes and thumbnails, opens shelved files, and frees thumbnails.
//!
//! One worker thread (a COM apartment, because the shell's thumbnail machinery needs one) sleeps in
//! `recv` until something arrives. The OLE drop callback only forwards the list of paths here, so
//! slow storage (a sleeping network share, a cloud placeholder) can never stall the UI thread. The
//! shelf holds *references*: nothing in this file creates, moves or deletes a user's file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notch_core::bus::BusSender;
use notch_core::draw::ImageId;
use notch_core::events::{EventKind, FileEntry, Source};
use notch_core::image::ImageCache;
use notch_core::module::ShelfCmd;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::win::dragdrop::ShelfSlot;
use crate::win::shellimg;
use crate::win::util::wide;

/// Thumbnails are requested at this size (drawn at 44 DIPs; 2x leaves room for high-DPI).
const THUMB_EDGE: i32 = 96;
/// Entries per `FileDropped` event.
const BATCH: usize = 50;

pub enum Req {
    /// Paths dropped on the notch.
    Dropped(Vec<PathBuf>),
    /// A file the iPhone sent, already saved in the inbox folder.
    Received(PathBuf),
    Open(String),
    Release(Vec<u64>),
    Quit,
}

pub struct ShelfService {
    tx: Sender<Req>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(1);

impl ShelfService {
    /// Start the worker and publish its inbox in `slot` so the OLE drop target can reach it.
    pub fn start(
        bus: BusSender,
        images: Arc<ImageCache>,
        slot: &ShelfSlot,
    ) -> Option<ShelfService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let thread = std::thread::Builder::new()
            .name("shelf".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(rx, bus, images);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the shelf worker: {e}"))
            .ok()?;
        if let Ok(mut g) = slot.lock() {
            *g = Some(tx.clone());
        }
        Some(ShelfService {
            tx,
            thread: Some(thread),
            done,
        })
    }

    pub fn command(&self, cmd: &ShelfCmd) {
        let req = match cmd {
            ShelfCmd::Open(p) => Req::Open(p.to_string()),
            ShelfCmd::Release(ids) => Req::Release(ids.clone()),
            // Dragging out runs OLE's modal loop on the UI thread; the app handles it.
            ShelfCmd::DragOut(_) => return,
        };
        let _ = self.tx.send(req);
    }

    /// A file the iPhone sent: it goes on the shelf like a dropped one, marked as from the phone.
    pub fn add_received(&self, path: PathBuf) {
        let _ = self.tx.send(Req::Received(path));
    }

    pub fn stop(mut self, slot: &ShelfSlot) {
        if let Ok(mut g) = slot.lock() {
            *g = None;
        }
        let _ = self.tx.send(Req::Quit);
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(300);
            while !self.done.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.done.load(Ordering::Acquire) {
                let _ = h.join();
            }
        }
    }
}

fn run(rx: Receiver<Req>, bus: BusSender, images: Arc<ImageCache>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    while let Ok(req) = rx.recv() {
        match req {
            Req::Quit => break,
            Req::Dropped(paths) => ingest(&paths, Source::Local, &bus, &images),
            Req::Received(path) => ingest(&[path], Source::Phone, &bus, &images),
            Req::Open(p) => open(&p),
            Req::Release(ids) => {
                for id in ids {
                    images.remove(ImageId(id));
                }
            }
        }
    }
    unsafe { CoUninitialize() };
}

/// The shelf's view of one path: name, size and kind, without ever failing on odd storage.
pub fn describe(path: &Path) -> (String, u64, bool) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        // A drive root ("C:\") has no file name.
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    match std::fs::metadata(path) {
        Ok(m) => (name, if m.is_dir() { 0 } else { m.len() }, m.is_dir()),
        Err(_) => (name, 0, false),
    }
}

fn ingest(paths: &[PathBuf], source: Source, bus: &BusSender, images: &ImageCache) {
    for chunk in paths.chunks(BATCH) {
        let entries: Vec<FileEntry> = chunk
            .iter()
            .map(|p| {
                let (name, size, is_dir) = describe(p);
                let thumb = shellimg::thumbnail(p, THUMB_EDGE)
                    .and_then(|img| images.put(img))
                    .map_or(0, |id| id.0);
                FileEntry {
                    id: NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed),
                    name: name.into(),
                    path: p.to_string_lossy().into_owned().into(),
                    size,
                    thumb,
                    is_dir,
                }
            })
            .collect();
        if !entries.is_empty() {
            bus.send(source, EventKind::FileDropped(entries));
        }
    }
}

fn open(path: &str) {
    let wpath = wide(path);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wpath.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describing_files_folders_and_missing_paths() {
        let dir = std::env::temp_dir().join(format!("shark-notch-shelf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.bin");
        std::fs::write(&file, vec![0u8; 1500]).unwrap();
        assert_eq!(describe(&file), ("data.bin".to_string(), 1500, false));
        let (name, size, is_dir) = describe(&dir);
        assert!(name.starts_with("shark-notch-shelf-") && size == 0 && is_dir);
        let missing = dir.join("nope.txt");
        assert_eq!(
            describe(&missing),
            ("nope.txt".to_string(), 0, false),
            "a vanished file is still shown"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drive_root_keeps_its_path_as_its_name() {
        assert_eq!(describe(Path::new("C:\\")).0, "C:\\");
    }
}
