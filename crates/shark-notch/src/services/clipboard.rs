//! The system clipboard as a bus producer, and the owner of clipboard-history *content*.
//!
//! A worker thread owns a message-only window registered with `AddClipboardFormatListener`, so it is
//! woken by Windows exactly when the clipboard changes (no polling, nothing hooked). It reads the
//! new content on that thread, never on the UI thread, and publishes small `ClipboardItem` events;
//! the full text and the full-size images stay here (`ClipStore`), and come back when the user clicks
//! an entry. Privacy rules: content that its owner marked as excluded from clipboard history (password
//! managers do) is skipped, history is never written to disk, only pinned *text* is (optional).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notch_core::bus::BusSender;
use notch_core::clipstore::{Added, ClipStore, Content, Entry, Limits, fnv1a64};
use notch_core::config::ClipboardCfg;
use notch_core::dib;
use notch_core::events::{EventKind, Source};
use notch_core::image::ImageCache;
use notch_core::module::ClipCmd;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_UNICODETEXT};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    KillTimer, MSG, PostMessageW, PostQuitMessage, SW_SHOWNORMAL, SetTimer, TranslateMessage,
    WINDOW_EX_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_DESTROY, WM_TIMER, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::win::util::wide;
use crate::win::{imaging, paths, textclip, window};

const CLASS: &str = "SharkNotchClipboard";
const WM_REQ: u32 = WM_APP + 21;
const TIMER_CAPTURE: usize = 1;
/// Apps often announce one copy several times in a row; read once, shortly after the last.
const CAPTURE_DELAY_MS: u32 = 60;
/// Never copy more than this out of the clipboard (a 96 MiB bitmap is ~5000x5000 pixels).
const MAX_IMAGE_BYTES: usize = 96 << 20;
const MAX_TEXT_BYTES: usize = 8 << 20;
const THUMB_EDGE: u32 = 128;

enum Req {
    Cmd(ClipCmd),
    Config(ClipboardCfg),
    /// An item that did not come from this PC's clipboard (the iPhone listener, phase 11).
    #[allow(dead_code)]
    AddText(Source, String),
    Suspend(bool),
    Quit,
}

pub struct ClipboardService {
    tx: Sender<Req>,
    hwnd: Arc<AtomicIsize>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

impl ClipboardService {
    pub fn start(
        cfg: ClipboardCfg,
        bus: BusSender,
        images: Arc<ImageCache>,
    ) -> Option<ClipboardService> {
        let (tx, rx) = channel();
        let hwnd = Arc::new(AtomicIsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = sync_channel::<bool>(1);
        let (h2, d2) = (hwnd.clone(), done.clone());
        let thread = std::thread::Builder::new()
            .name("clipboard".into())
            .stack_size(1024 * 1024)
            .spawn(move || {
                run(cfg, bus, images, rx, h2, ready_tx);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the clipboard worker: {e}"))
            .ok()?;
        // Wait (briefly) for the window, so that commands sent right away have somewhere to go.
        if !ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false)
        {
            crate::warn!("the clipboard worker did not start; clipboard history is off");
            return None;
        }
        Some(ClipboardService {
            tx,
            hwnd,
            thread: Some(thread),
            done,
        })
    }

    fn post(&self, req: Req) {
        if self.tx.send(req).is_ok() {
            let h = self.hwnd.load(Ordering::Acquire);
            if h != 0 {
                unsafe {
                    let _ = PostMessageW(Some(HWND(h as *mut _)), WM_REQ, WPARAM(0), LPARAM(0));
                }
            }
        }
    }

    pub fn command(&self, cmd: ClipCmd) {
        self.post(Req::Cmd(cmd));
    }

    pub fn configure(&self, cfg: ClipboardCfg) {
        self.post(Req::Config(cfg));
    }

    /// Add text that arrived from somewhere other than this PC's clipboard (the iPhone).
    #[allow(dead_code)] // used by the iPhone listener (phase 11)
    pub fn add_text(&self, source: Source, text: String) {
        self.post(Req::AddText(source, text));
    }

    pub fn suspend(&self, on: bool) {
        self.post(Req::Suspend(on));
    }

    pub fn stop(mut self) {
        self.post(Req::Quit);
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(400);
            while !self.done.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.done.load(Ordering::Acquire) {
                let _ = h.join();
            }
        }
    }
}

/// Clipboard formats registered by name (their ids are only known at run time).
#[derive(Clone, Copy)]
struct Formats {
    png: u32,
    /// Set by password managers and similar: "do not record this".
    exclude: u32,
    /// A DWORD: 0 means "do not put this into clipboard history".
    history_ok: u32,
}

impl Formats {
    fn register() -> Formats {
        unsafe {
            Formats {
                png: RegisterClipboardFormatW(w!("PNG")),
                exclude: RegisterClipboardFormatW(w!(
                    "ExcludeClipboardContentFromMonitorProcessing"
                )),
                history_ok: RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")),
            }
        }
    }
}

struct Worker {
    hwnd: HWND,
    rx: Receiver<Req>,
    bus: BusSender,
    images: Arc<ImageCache>,
    cfg: ClipboardCfg,
    store: ClipStore,
    fmts: Formats,
    blob_dir: PathBuf,
    pins_path: PathBuf,
    next_blob: u64,
    /// The sequence number of the clipboard state *we* produced; updates for it are ignored.
    own_seq: u32,
    last_seq: u32,
    paused: bool,
    dirty: bool,
}

thread_local! {
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
}

fn with_worker<R>(f: impl FnOnce(&mut Worker) -> R) -> Option<R> {
    WORKER.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn limits(cfg: &ClipboardCfg) -> Limits {
    Limits {
        max_items: cfg.max_items as usize,
        max_images: if cfg.capture_images {
            cfg.max_images as usize
        } else {
            0
        },
        max_text_bytes: cfg.max_text_kib as usize * 1024,
    }
}

fn run(
    cfg: ClipboardCfg,
    bus: BusSender,
    images: Arc<ImageCache>,
    rx: Receiver<Req>,
    hwnd_cell: Arc<AtomicIsize>,
    ready: std::sync::mpsc::SyncSender<bool>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            crate::warn!("clipboard: cannot create the listener window: {e}");
            let _ = ready.send(false);
            unsafe { CoUninitialize() };
            return;
        }
    };
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        crate::warn!("clipboard: AddClipboardFormatListener failed: {e}");
        unsafe {
            let _ = DestroyWindow(hwnd);
            CoUninitialize();
        }
        let _ = ready.send(false);
        return;
    }
    let data = paths::data_dir();
    let blob_dir = data.join("clips");
    // Images from a previous run are never reused (history is memory-only): start clean.
    let _ = std::fs::remove_dir_all(&blob_dir);
    let _ = std::fs::create_dir_all(&blob_dir);
    let mut w = Worker {
        hwnd,
        rx,
        bus,
        images,
        store: ClipStore::new(limits(&cfg)),
        cfg,
        fmts: Formats::register(),
        blob_dir,
        pins_path: data.join("pins.json"),
        next_blob: 1,
        own_seq: 0,
        last_seq: 0,
        paused: false,
        dirty: false,
    };
    w.load_pins();
    WORKER.with(|c| *c.borrow_mut() = Some(w));
    hwnd_cell.store(hwnd.0 as isize, Ordering::Release);
    let _ = ready.send(true);

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    hwnd_cell.store(0, Ordering::Release);
    if let Some(w) = WORKER.with(|c| c.borrow_mut().take()) {
        let _ = std::fs::remove_dir_all(&w.blob_dir);
    }
    unsafe { CoUninitialize() };
}

fn create_window() -> windows::core::Result<HWND> {
    window::register_class(CLASS, proc)?;
    unsafe {
        let hinst = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
        let class = wide(CLASS);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            w!("Shark Notch clipboard listener"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            with_worker(|w| w.on_update());
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_CAPTURE => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_CAPTURE);
            }
            with_worker(|w| w.capture());
            LRESULT(0)
        }
        WM_REQ => {
            with_worker(|w| w.drain());
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = RemoveClipboardFormatListener(hwnd);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ----- reading the clipboard ----------------------------------------------------------------------

enum Read {
    /// Nothing we keep (excluded, empty, unsupported, or the clipboard stayed locked).
    Skip,
    Text(String),
    Image {
        bytes: Vec<u8>,
        png: bool,
    },
}

/// Open the clipboard, retrying briefly: another process may be holding it right after a copy.
unsafe fn open_with_retry(hwnd: HWND) -> bool {
    for _ in 0..8 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

/// Copy a global-memory clipboard handle's bytes out (bounded by `max`).
unsafe fn read_global(h: HANDLE, max: usize) -> Option<Vec<u8>> {
    let hg = HGLOBAL(h.0);
    let size = unsafe { GlobalSize(hg) };
    if size == 0 || size > max {
        return None;
    }
    let p = unsafe { GlobalLock(hg) } as *const u8;
    if p.is_null() {
        return None;
    }
    let v = unsafe { std::slice::from_raw_parts(p, size) }.to_vec();
    unsafe {
        let _ = GlobalUnlock(hg);
    }
    Some(v)
}

fn read_clipboard(hwnd: HWND, f: &Formats, want_images: bool) -> Read {
    unsafe {
        if !open_with_retry(hwnd) {
            return Read::Skip;
        }
        let result = (|| {
            // The owner asked not to be recorded (password managers, banking apps, ...).
            if IsClipboardFormatAvailable(f.exclude).is_ok() {
                return Read::Skip;
            }
            if IsClipboardFormatAvailable(f.history_ok).is_ok()
                && let Ok(h) = GetClipboardData(f.history_ok)
                && let Some(b) = read_global(h, 16)
                && b.len() >= 4
                && u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == 0
            {
                return Read::Skip;
            }
            if IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)).is_ok()
                && let Ok(h) = GetClipboardData(u32::from(CF_UNICODETEXT.0))
                && let Some(raw) = read_global(h, MAX_TEXT_BYTES * 2)
            {
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&u| u != 0)
                    .collect();
                let text = String::from_utf16_lossy(&units);
                if !text.trim().is_empty() {
                    return Read::Text(text);
                }
            }
            if want_images {
                if IsClipboardFormatAvailable(f.png).is_ok()
                    && let Ok(h) = GetClipboardData(f.png)
                    && let Some(bytes) = read_global(h, MAX_IMAGE_BYTES)
                {
                    return Read::Image { bytes, png: true };
                }
                for fmt in [CF_DIBV5, CF_DIB] {
                    if IsClipboardFormatAvailable(u32::from(fmt.0)).is_ok()
                        && let Ok(h) = GetClipboardData(u32::from(fmt.0))
                        && let Some(bytes) = read_global(h, MAX_IMAGE_BYTES)
                    {
                        return Read::Image { bytes, png: false };
                    }
                }
            }
            Read::Skip
        })();
        let _ = CloseClipboard();
        result
    }
}

// ----- writing the clipboard ----------------------------------------------------------------------

/// Put `data` on the (already open, already emptied) clipboard as format `fmt`.
unsafe fn set_bytes(fmt: u32, data: &[u8]) -> bool {
    unsafe {
        let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, data.len()) else {
            return false;
        };
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        let _ = GlobalUnlock(h);
        if SetClipboardData(fmt, Some(HANDLE(h.0))).is_err() {
            let _ = GlobalFree(Some(h));
            return false;
        }
        true
    }
}

/// Replace the clipboard with an image: PNG for modern apps, a flattened DIB for old ones.
fn set_image(owner: HWND, f: &Formats, png: &[u8]) -> bool {
    let Some(dib) =
        imaging::decode(png, 16_384).and_then(|i| dib::dib_from_bgra(i.w, i.h, &i.bgra))
    else {
        return false;
    };
    unsafe {
        if !open_with_retry(owner) {
            return false;
        }
        let ok = EmptyClipboard().is_ok()
            && set_bytes(f.png, png)
            && set_bytes(u32::from(CF_DIB.0), &dib);
        let _ = CloseClipboard();
        ok
    }
}

/// For the self-test: put a bitmap on the clipboard as `CF_DIB`.
pub fn put_dib(owner: HWND, w: u32, h: u32, bgra_premultiplied: &[u8]) -> bool {
    let Some(dib) = dib::dib_from_bgra(w, h, bgra_premultiplied) else {
        return false;
    };
    unsafe {
        if !open_with_retry(owner) {
            return false;
        }
        let ok = EmptyClipboard().is_ok() && set_bytes(u32::from(CF_DIB.0), &dib);
        let _ = CloseClipboard();
        ok
    }
}

/// For the self-test: put text on the clipboard flagged "exclude from monitoring" (as a password
/// manager would); the service must ignore it.
pub fn put_excluded_text(owner: HWND, text: &str) -> bool {
    let wt = wide(text);
    let bytes: Vec<u8> = wt.iter().flat_map(|u| u.to_le_bytes()).collect();
    unsafe {
        if !open_with_retry(owner) {
            return false;
        }
        let fmt = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
        let ok = EmptyClipboard().is_ok()
            && set_bytes(u32::from(CF_UNICODETEXT.0), &bytes)
            && set_bytes(fmt, &[1, 0, 0, 0]);
        let _ = CloseClipboard();
        ok
    }
}

/// For the self-test: the text currently on the clipboard.
pub fn current_text(owner: HWND) -> Option<String> {
    match read_clipboard(owner, &Formats::register(), false) {
        Read::Text(t) => Some(t),
        _ => None,
    }
}

// ----- the worker's behaviour ---------------------------------------------------------------------

impl Worker {
    fn on_update(&mut self) {
        if self.paused {
            self.dirty = true;
        } else {
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_CAPTURE, CAPTURE_DELAY_MS, None);
            }
        }
    }

    fn emit_item(&self, id: u64, source: Source) {
        if let Some(e) = self.store.get(id) {
            self.bus.send(source, EventKind::ClipboardItem(e.item()));
        }
    }

    /// Free what an entry held and tell the UI it is gone.
    fn release(&mut self, e: Entry) {
        if let Content::Image { blob, .. } = e.content {
            let _ = std::fs::remove_file(self.blob_path(blob));
        }
        if let Some(t) = e.thumb {
            self.images.remove(t);
        }
        self.bus
            .send(Source::Local, EventKind::ClipboardRemoved(e.id));
    }

    fn blob_path(&self, blob: u64) -> PathBuf {
        self.blob_dir.join(format!("{blob}.png"))
    }

    fn evict(&mut self) {
        for e in self.store.evict() {
            self.release(e);
        }
    }

    fn capture(&mut self) {
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == self.own_seq || seq == self.last_seq {
            return;
        }
        self.last_seq = seq;
        match read_clipboard(self.hwnd, &self.fmts, self.cfg.capture_images) {
            Read::Skip => {}
            Read::Text(t) => self.add_text(Source::Local, &t),
            Read::Image { bytes, png } => self.add_image(bytes, png),
        }
    }

    fn add_text(&mut self, source: Source, text: &str) {
        match self.store.add_text(source, text) {
            Added::New(id) | Added::Bumped(id) => {
                self.emit_item(id, source);
                self.evict();
            }
            Added::Rejected(_) => {}
        }
    }

    fn add_image(&mut self, bytes: Vec<u8>, png: bool) {
        let hash = fnv1a64(&bytes);
        if let Some(id) = self.store.find(hash, true) {
            self.store.touch(id);
            self.emit_item(id, Source::Local);
            return;
        }
        // WIC reads PNG directly; a clipboard DIB only needs a file header in front of it.
        let src = if png {
            bytes
        } else {
            match dib::bmp_from_dib(&bytes) {
                Some(b) => b,
                None => return,
            }
        };
        let Some((thumb, w, h)) = imaging::decode_sized(&src, THUMB_EDGE) else {
            return;
        };
        let blob = self.next_blob;
        self.next_blob += 1;
        let path = self.blob_path(blob);
        let stored = if png {
            std::fs::write(&path, &src).is_ok()
        } else {
            imaging::encode_png_file(&src, &path)
        };
        if !stored {
            return;
        }
        let thumb_id = self.images.put(thumb);
        match self
            .store
            .add_image(Source::Local, hash, w, h, blob, thumb_id)
        {
            Added::New(id) => {
                self.emit_item(id, Source::Local);
                self.evict();
            }
            Added::Bumped(id) => {
                // Cannot normally happen (checked above) but keep the books straight.
                let _ = std::fs::remove_file(&path);
                if let Some(t) = thumb_id {
                    self.images.remove(t);
                }
                self.emit_item(id, Source::Local);
            }
            Added::Rejected(_) => {
                let _ = std::fs::remove_file(&path);
                if let Some(t) = thumb_id {
                    self.images.remove(t);
                }
            }
        }
    }

    // ----- commands -------------------------------------------------------------------------------

    fn drain(&mut self) {
        while let Ok(req) = self.rx.try_recv() {
            match req {
                Req::Cmd(c) => self.execute(c),
                Req::Config(cfg) => {
                    self.store.limits = limits(&cfg);
                    let persist_changed = cfg.persist_pins != self.cfg.persist_pins;
                    self.cfg = cfg;
                    self.evict();
                    if persist_changed {
                        self.save_pins();
                    }
                }
                Req::AddText(source, text) => self.add_text(source, &text),
                Req::Suspend(on) => {
                    self.paused = on;
                    if !on && std::mem::take(&mut self.dirty) {
                        self.on_update();
                    }
                }
                Req::Quit => unsafe {
                    let _ = DestroyWindow(self.hwnd);
                },
            }
        }
    }

    fn execute(&mut self, cmd: ClipCmd) {
        match cmd {
            ClipCmd::Copy(id) => self.copy_back(id),
            ClipCmd::Pin(id, pinned) => {
                if self.store.set_pinned(id, pinned) {
                    self.save_pins();
                }
            }
            ClipCmd::Remove(id) => {
                if let Some(e) = self.store.remove(id) {
                    let was_pinned = e.pinned;
                    self.release(e);
                    if was_pinned {
                        self.save_pins();
                    }
                }
            }
            ClipCmd::Clear => {
                for e in self.store.clear_unpinned() {
                    self.release(e);
                }
            }
            ClipCmd::Open(id) => self.open_link(id),
        }
    }

    /// Put an entry back on the system clipboard (and mark it as most recently used).
    fn copy_back(&mut self, id: u64) {
        let Some(e) = self.store.get(id) else { return };
        let ok = match &e.content {
            Content::Text(t) => textclip::copy_text(self.hwnd, t),
            Content::Image { blob, .. } => match std::fs::read(self.blob_path(*blob)) {
                Ok(png) => set_image(self.hwnd, &self.fmts, &png),
                Err(_) => false,
            },
        };
        if !ok {
            crate::warn!("clipboard: could not put entry {id} back on the clipboard");
            return;
        }
        // Our own write must not come back as a "new" copy.
        self.own_seq = unsafe { GetClipboardSequenceNumber() };
        self.store.touch(id);
        self.emit_item(id, Source::Local);
    }

    fn open_link(&self, id: u64) {
        let Some(Entry {
            content: Content::Text(t),
            ..
        }) = self.store.get(id)
        else {
            return;
        };
        let t = t.trim();
        let url = if t.to_ascii_lowercase().starts_with("www.") {
            format!("https://{t}")
        } else {
            t.to_string()
        };
        let lower = url.to_ascii_lowercase();
        // Only web links are ever launched from here; anything else stays a copy-only entry.
        if !(lower.starts_with("https://") || lower.starts_with("http://")) {
            crate::warn!("clipboard: refused to open a non-web link");
            return;
        }
        let wurl = wide(&url);
        unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(wurl.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    // ----- pinned text on disk -------------------------------------------------------------------

    fn load_pins(&mut self) {
        if !self.cfg.persist_pins {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&self.pins_path) else {
            return;
        };
        let n = self.store.load_pins(&text);
        if n > 0 {
            crate::info!("clipboard: restored {n} pinned item(s)");
            let ids: Vec<u64> = self.store.entries().iter().rev().map(|e| e.id).collect();
            for id in ids {
                self.emit_item(id, Source::Local);
            }
        }
    }

    fn save_pins(&self) {
        if !self.cfg.persist_pins {
            let _ = std::fs::remove_file(&self.pins_path);
            return;
        }
        let text = self.store.pins_to_json();
        let has_pins = self
            .store
            .entries()
            .iter()
            .any(|e| e.pinned && matches!(e.content, Content::Text(_)));
        if !has_pins {
            let _ = std::fs::remove_file(&self.pins_path);
            return;
        }
        if let Some(dir) = self.pins_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        write_atomically(&self.pins_path, text.as_bytes());
    }
}

/// Write to a temporary file next to `path`, then rename over it: a crash never leaves half a file.
fn write_atomically(path: &Path, data: &[u8]) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, data).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}
