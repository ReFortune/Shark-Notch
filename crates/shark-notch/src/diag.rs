//! Self-test: a scripted run of the **real** app (real windows, the real GPU stack or WARP, the real
//! loop) that prints a report. CI runs it on a Windows runner; you can run it on your own machine
//! with `shark-notch.exe --selftest` (add `--no-exclude --light-probe` to enable the screen-pixel
//! probe) to get the numbers that matter for *your* GPU driver.
//!
//! What it measures:
//! * frame-time bursts for expand / page switch / collapse (hitch = interval > 1.5x refresh);
//! * idle CPU% and private working set with the GPU warm and with it released;
//! * the cost of warming the GPU stack from cold;
//! * (optionally) that the notch is actually on screen: a GDI read of the composed desktop.

use std::fmt::Write as _;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use notch_core::compose;
use notch_core::draw::DrawCmd;
use notch_core::events::{ClipKind, EventKind, Kind, MediaSnapshot, Notification, Source};
use notch_core::image::ImageData;
use notch_core::module::{ClipCmd, Command};
use notch_core::modules::media::Layout as MediaLayout;
use notch_core::shell::{Presence, Trigger};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};

use crate::app::{App, T_SCRIPT};
use crate::services::clipboard as clip;
use crate::services::privacy;
use crate::win::clock;
use crate::win::sys::{self, ProcMetrics};
use crate::win::{dragdrop, paths, textclip};

#[derive(Clone, Copy)]
struct Mark {
    t: f64,
    m: ProcMetrics,
}

/// A watchdog thread that sleeps in short steps and notes every time it woke up much later than it
/// asked: if *no* thread of this process ran for a while, the operating system (or the VM) stalled,
/// and a hitch in the frame report is not the app's fault. Reported at the end of the self-test.
struct Heartbeat {
    stalls: Arc<Mutex<Vec<(f64, f64)>>>,
}

impl Heartbeat {
    fn start() -> Heartbeat {
        let stalls = Arc::new(Mutex::new(Vec::new()));
        let s = stalls.clone();
        let _ = std::thread::Builder::new()
            .name("selftest-heartbeat".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                loop {
                    let t0 = clock::now();
                    std::thread::sleep(Duration::from_millis(15));
                    let dt = clock::now() - t0;
                    if dt > 0.12
                        && let Ok(mut v) = s.lock()
                    {
                        v.push((t0, dt));
                    }
                }
            });
        Heartbeat { stalls }
    }

    fn report(&self, start: f64) -> String {
        let v = self.stalls.lock().map(|g| g.clone()).unwrap_or_default();
        if v.is_empty() {
            return "no system stalls: a helper thread was never delayed by more than 120 ms"
                .into();
        }
        let list: Vec<String> = v
            .iter()
            .map(|(t, d)| format!("{:.0} ms at +{:.2}s", d * 1000.0, t - start))
            .collect();
        format!(
            "{} system stall(s) where even an idle helper thread woke late (the OS/VM, not the notch): {}",
            v.len(),
            list.join(", ")
        )
    }
}

pub struct SelfTest {
    start: f64,
    heartbeat: Heartbeat,
    next: usize,
    pub finished: bool,
    pub exit_code: i32,
    lines: Vec<String>,
    failures: Vec<String>,
    idle_begin: Option<Mark>,
    /// Id of the synthetic album art in the image cache.
    art_id: u64,
    /// `ClipboardItem` events seen before the current clipboard act.
    clip_mark: u32,
    clip_text_id: u64,
    /// Bus-event counts before the shelf drop: `(DragHover, FileDropped)`.
    shelf_mark: (u32, u32),
    shelf_dir: Option<std::path::PathBuf>,
    /// `Notification` events seen before the notification act in progress.
    notif_mark: u32,
    /// Frames presented when the summary banner was checked.
    frames_mark: u64,
    /// `Calendar` events seen before the calendar act in progress.
    cal_mark: u32,
    /// The scratch "Downloads" folder of the live-activities scenario, and the bus-event counts
    /// (`Downloads`, `DownloadDone`) before its acts.
    dl_dir: Option<std::path::PathBuf>,
    dl_mark: (u32, u32),
    /// The stats page scenario: `Stats` events seen at a point, and the start of the CPU window.
    stats_mark: u32,
    stats_begin: Option<Mark>,
    /// Cycles per second of a busy core (calibrated), for tick-free CPU percentages.
    cycles_hz: f64,
    warm_idle: Option<(f64, f64)>,     // (cpu %, private MiB)
    released_idle: Option<(f64, f64)>, // (cpu %, private MiB)
}

#[derive(Clone, Copy, Debug)]
enum Act {
    Baseline,
    ProbePill,
    Expand,
    ProbeExpanded,
    Scroll,
    Collapse,
    IdleBegin,
    IdleEndWarm,
    Release,
    IdleBeginReleased,
    IdleEndReleased,
    Warm,
    ExpandHover,
    Peek,
    MediaFeed,
    MediaExpand,
    ProbeMedia,
    MediaNext,
    CheckPeek,
    MediaGone,
    CheckGone,
    ClipText,
    ClipTextCheck,
    ClipLink,
    ClipLinkCheck,
    ClipImage,
    ClipImageCheck,
    ClipExcluded,
    ClipExcludedCheck,
    ClipRecopy,
    ClipRecopyCheck,
    ClipPin,
    ClipPinCheck,
    ClipUnpin,
    ClipUnpinCheck,
    ClipPage,
    ClipProbe,
    ShelfDrop,
    ShelfCheck,
    ShelfPage,
    ShelfProbe,
    ShelfOff,
    ShelfOffCheck,
    NotifAccess,
    NotifFeed,
    NotifCheckPeek,
    NotifAway,
    NotifAwayCheck,
    NotifBack,
    NotifBackCheck,
    NotifChipCheck,
    NotifOpen,
    NotifOpenCheck,
    NotifClose,
    NotifReleaseCheck,
    CalSetup,
    CalCheck,
    CalJoin,
    CalJoinCheck,
    CalPage,
    CalPageCheck,
    CalClose,
    CalChipCheck,
    CalOff,
    PomoSetup,
    PomoPage,
    PomoStart,
    PomoRunCheck,
    PomoEndCheck,
    PomoAddOpen,
    PomoAdd,
    PomoAddCheck,
    PomoSaveCheck,
    PomoOff,
    LivePage,
    LiveClick,
    LiveClickCheck,
    LiveCancel,
    LiveCancelCheck,
    LiveClose,
    LiveTimerLoad,
    LiveTimerChipCheck,
    LiveTimerEndCheck,
    LiveTimerClose,
    PrivacyOn,
    PrivacyCheck,
    PrivacyOff,
    PrivacyOffCheck,
    DlSetup,
    DlCheck,
    DlGrow,
    DlGrowCheck,
    DlFinish,
    DlFinishCheck,
    DlShow,
    DlShowCheck,
    DlOff,
    StatsPage,
    StatsBegin,
    StatsCheck,
    StatsClose,
    StatsSettled,
    StatsQuietCheck,
    Report,
}

const SCRIPT: &[(f64, Act)] = &[
    (1.0, Act::Baseline),
    (1.4, Act::ProbePill),
    (1.8, Act::Expand),
    (3.0, Act::ProbeExpanded),
    (3.4, Act::Scroll),
    (4.4, Act::Scroll),
    (5.4, Act::Collapse),
    (7.0, Act::IdleBegin),
    (12.0, Act::IdleEndWarm),
    (12.1, Act::Release),
    (13.2, Act::IdleBeginReleased),
    (18.2, Act::IdleEndReleased),
    (18.3, Act::Warm),
    (19.2, Act::ExpandHover),
    (20.8, Act::Collapse),
    (21.8, Act::Peek),
    (24.5, Act::MediaFeed),
    (24.9, Act::MediaExpand),
    (26.4, Act::ProbeMedia),
    (26.6, Act::Collapse),
    (28.0, Act::MediaNext),
    (28.7, Act::CheckPeek),
    (30.5, Act::MediaGone),
    (31.0, Act::CheckGone),
    (31.5, Act::ClipText),
    (32.3, Act::ClipTextCheck),
    (32.4, Act::ClipLink),
    (33.1, Act::ClipLinkCheck),
    (33.2, Act::ClipImage),
    (34.2, Act::ClipImageCheck),
    (34.3, Act::ClipExcluded),
    (35.0, Act::ClipExcludedCheck),
    (35.1, Act::ClipRecopy),
    (35.8, Act::ClipRecopyCheck),
    (35.9, Act::ClipPin),
    (36.5, Act::ClipPinCheck),
    (36.6, Act::ClipUnpin),
    (37.2, Act::ClipUnpinCheck),
    (37.3, Act::ClipPage),
    (38.8, Act::ClipProbe),
    (39.3, Act::ShelfDrop),
    (40.3, Act::ShelfCheck),
    (40.4, Act::ShelfPage),
    (41.9, Act::ShelfProbe),
    (42.5, Act::ShelfOff),
    (43.0, Act::ShelfOffCheck),
    (43.6, Act::NotifAccess),
    (43.8, Act::NotifFeed),
    (44.6, Act::NotifCheckPeek),
    (44.7, Act::NotifAway),
    (45.6, Act::NotifAwayCheck),
    (45.7, Act::NotifBack),
    (46.6, Act::NotifBackCheck),
    (50.4, Act::NotifChipCheck),
    (50.6, Act::NotifOpen),
    (52.1, Act::NotifOpenCheck),
    (52.2, Act::NotifClose),
    (53.8, Act::NotifReleaseCheck),
    (54.4, Act::CalSetup),
    (56.0, Act::CalCheck),
    (56.2, Act::CalJoin),
    (56.5, Act::CalJoinCheck),
    (56.6, Act::CalPage),
    (58.1, Act::CalPageCheck),
    (58.2, Act::CalClose),
    (59.6, Act::CalChipCheck),
    (59.8, Act::CalOff),
    (60.4, Act::PomoSetup),
    (60.6, Act::PomoPage),
    (62.1, Act::PomoStart),
    (62.6, Act::PomoRunCheck),
    (66.8, Act::PomoEndCheck),
    (67.0, Act::PomoAddOpen),
    (68.5, Act::PomoAdd),
    (68.8, Act::PomoAddCheck),
    (70.0, Act::PomoSaveCheck),
    (70.2, Act::PomoOff),
    (70.6, Act::LivePage),
    (72.0, Act::LiveClick),
    (72.3, Act::LiveClickCheck),
    (72.4, Act::LiveCancel),
    (72.7, Act::LiveCancelCheck),
    (72.8, Act::LiveClose),
    (73.0, Act::LiveTimerLoad),
    (73.8, Act::LiveTimerChipCheck),
    (76.6, Act::LiveTimerEndCheck),
    (76.8, Act::LiveTimerClose),
    (77.4, Act::PrivacyOn),
    (79.4, Act::PrivacyCheck),
    (79.5, Act::PrivacyOff),
    (81.5, Act::PrivacyOffCheck),
    (82.0, Act::DlSetup),
    (83.8, Act::DlCheck),
    (83.9, Act::DlGrow),
    (85.2, Act::DlGrowCheck),
    (85.3, Act::DlFinish),
    (86.3, Act::DlFinishCheck),
    (86.4, Act::DlShow),
    (86.6, Act::DlShowCheck),
    (86.8, Act::DlOff),
    (87.2, Act::StatsPage),
    (88.2, Act::StatsBegin),
    (91.4, Act::StatsCheck),
    (91.5, Act::StatsClose),
    (92.2, Act::StatsSettled),
    (94.8, Act::StatsQuietCheck),
    (95.2, Act::Report),
];

const CLIP_TEXT: &str = "Selftest clipboard text";
const CLIP_LINK: &str = "https://example.com/shark-notch-selftest";
/// The colour of the synthetic clipboard image (r, g, b).
const CLIP_RGB: (u8, u8, u8) = (30, 160, 90);

/// The colour of the synthetic album art (r, g, b): distinctive and not a theme colour.
const ART_RGB: (u8, u8, u8) = (210, 60, 120);

fn demo_snapshot(title: &str, art: u64) -> MediaSnapshot {
    MediaSnapshot {
        app: "Selftest".into(),
        title: title.into(),
        artist: "Shark Notch".into(),
        album: "Diagnostics".into(),
        playing: true,
        position_ms: 42_000,
        duration_ms: 180_000,
        art,
        accent: Some([ART_RGB.0, ART_RGB.1, ART_RGB.2]),
        can_play_pause: true,
        can_next: true,
        can_prev: true,
        can_seek: true,
    }
}

fn mark() -> Mark {
    Mark {
        t: clock::now(),
        m: sys::proc_metrics(),
    }
}

impl SelfTest {
    fn say(&mut self, line: String) {
        println!("SELFTEST {line}");
        crate::info!("selftest: {line}");
        self.lines.push(line);
    }

    fn fail(&mut self, why: String) {
        self.say(format!("FAIL {why}"));
        self.failures.push(why);
    }
}

/// Read a pixel of the composed desktop (what you would see), as `(r, g, b)`.
fn probe_pixel(x: i32, y: i32) -> Option<(u8, u8, u8)> {
    unsafe {
        let dc = GetDC(None);
        let c: COLORREF = GetPixel(dc, x, y);
        ReleaseDC(None, dc);
        (c.0 != 0xFFFF_FFFF).then_some((
            (c.0 & 0xFF) as u8,
            ((c.0 >> 8) & 0xFF) as u8,
            ((c.0 >> 16) & 0xFF) as u8,
        ))
    }
}

/// A synthetic notification as the phone transport would deliver it (ids with bit 32 set).
fn demo_note(n: u64, title: &str, body: &str) -> EventKind {
    EventKind::Notification(Notification {
        id: (1 << 32) | n,
        app: "Selftest Mail".into(),
        title: title.into(),
        body: body.into(),
        icon: 0,
        fresh: true,
        ago_secs: 0,
        quiet: false,
    })
}

/// Every string in the display list of the last frame (what is on screen, as text).
fn drawn_text(a: &App) -> Vec<String> {
    a.list
        .cmds
        .iter()
        .filter_map(|c| match c {
            DrawCmd::Text { text, .. } => Some(text.as_str().to_string()),
            _ => None,
        })
        .collect()
}

/// The application manifest embedded in this exe (resource `RT_MANIFEST` #1), if there is one.
fn embedded_manifest() -> Option<String> {
    use windows::Win32::System::LibraryLoader::{
        FindResourceW, GetModuleHandleW, LoadResource, LockResource, SizeofResource,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CREATEPROCESS_MANIFEST_RESOURCE_ID, RT_MANIFEST,
    };
    use windows::core::PCWSTR;
    unsafe {
        let module = GetModuleHandleW(None).ok()?;
        // MAKEINTRESOURCE(CREATEPROCESS_MANIFEST_RESOURCE_ID): an integer id passed in a string slot.
        let id = PCWSTR(CREATEPROCESS_MANIFEST_RESOURCE_ID as usize as *const u16);
        let res = FindResourceW(Some(module), id, RT_MANIFEST);
        if res.is_invalid() {
            return None;
        }
        let size = SizeofResource(Some(module), res) as usize;
        let data = LoadResource(Some(module), res).ok()?;
        let ptr = LockResource(data) as *const u8;
        if ptr.is_null() || size == 0 {
            return None;
        }
        Some(String::from_utf8_lossy(std::slice::from_raw_parts(ptr, size)).into_owned())
    }
}

/// `YYYYMMDDTHHMMSSZ` for a Unix time.
fn ics_stamp(unix: i64) -> String {
    let (y, mo, d, h, mi, s) = notch_core::civil::civil_from_unix(unix);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A real mouse click on the region `id` of the last frame, through the app's own mouse handler.
fn click_region(a: &mut App, id: u32) -> bool {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};
    let Some(region) = a.list.hits.iter().find(|h| h.id.0 == id).map(|h| h.rect) else {
        return false;
    };
    let c = region.center();
    let ppd = a.layout.px_per_dip;
    let (x, y) = ((c.x * ppd).round() as isize, (c.y * ppd).round() as isize);
    let lp = LPARAM(((y & 0xFFFF) << 16) | (x & 0xFFFF));
    a.on_stage_mouse(WM_MOUSEMOVE, WPARAM(0), lp);
    a.on_stage_mouse(WM_LBUTTONDOWN, WPARAM(1), lp);
    a.on_stage_mouse(WM_LBUTTONUP, WPARAM(0), lp);
    true
}

/// Apply a modified copy of the running configuration (as if the user had edited the file).
fn reconfigure(a: &mut App, edit: impl FnOnce(&mut notch_core::config::Config)) {
    let mut cfg = a.cfg.clone();
    edit(&mut cfg);
    a.apply_config(notch_core::config::Loaded {
        config: cfg,
        warnings: Vec::new(),
    });
}

fn clip_events(a: &App) -> u32 {
    a.bus_counts[Kind::ClipboardItem as usize]
}

/// Did a `ClipboardItem` event arrive since `st.clip_mark`? Returns it if so.
fn new_clip_item(a: &App, st: &SelfTest) -> Option<notch_core::events::ClipboardItem> {
    (clip_events(a) > st.clip_mark)
        .then(|| a.last_clip.clone())
        .flatten()
}

fn pins_file() -> std::path::PathBuf {
    paths::data_dir().join("pins.json")
}

/// Row `k`'s thumbnail centre in screen pixels, computed the same way the clipboard page lays out.
fn clip_thumb_pixel(a: &App, page: usize, k: usize) -> (i32, i32) {
    use notch_core::modules::clipboard::{HEADER_H, ROW_H};
    let size = a.pages[page];
    let area = compose::content_rect(0.0, size, a.layout.win_dip.w, &a.metrics);
    let (_, rest) = area.split_top(HEADER_H);
    let list_y = rest.y + 4.0;
    let (rx, ry) = (
        area.x + 6.0 + 14.0,
        list_y + k as f32 * ROW_H + (ROW_H - 4.0) * 0.5,
    );
    let (wx, wy, _, _) = a.layout.win_px;
    let ppd = a.layout.px_per_dip;
    (
        wx + (rx * ppd).round() as i32,
        wy + (ry * ppd).round() as i32,
    )
}

/// Drive the notch's real OLE drop target the way the system does for a drag: enter, hover, drop.
/// Returns the effects it answered with.
fn simulate_drop(a: &App, data: &windows::Win32::System::Com::IDataObject) -> Option<[u32; 3]> {
    use windows::Win32::Foundation::POINTL;
    use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_NONE};
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
    let target = a.drop_target.as_ref()?;
    let (pt, keys) = (POINTL { x: 0, y: 0 }, MODIFIERKEYS_FLAGS(MK_LBUTTON.0));
    let mut e = [DROPEFFECT_NONE; 3];
    unsafe {
        let mut eff: DROPEFFECT = DROPEFFECT(0x7);
        target.DragEnter(data, keys, pt, &mut eff).ok()?;
        e[0] = eff;
        eff = DROPEFFECT(0x7);
        target.DragOver(keys, pt, &mut eff).ok()?;
        e[1] = eff;
        eff = DROPEFFECT(0x7);
        target.Drop(data, keys, pt, &mut eff).ok()?;
        e[2] = eff;
    }
    Some([e[0].0, e[1].0, e[2].0])
}

pub fn begin(a: &mut App) {
    let now = clock::now();
    if a.opts.light_probe {
        a.cfg.appearance.theme = notch_core::theme::ThemeMode::Light;
        a.refresh_theme();
    }
    let mut st = SelfTest {
        start: now,
        heartbeat: Heartbeat::start(),
        next: 0,
        finished: false,
        exit_code: 0,
        lines: Vec::new(),
        failures: Vec::new(),
        idle_begin: None,
        art_id: 0,
        clip_mark: 0,
        clip_text_id: 0,
        shelf_mark: (0, 0),
        shelf_dir: None,
        notif_mark: 0,
        frames_mark: 0,
        cal_mark: 0,
        dl_dir: None,
        dl_mark: (0, 0),
        stats_mark: 0,
        stats_begin: None,
        cycles_hz: sys::cycles_per_sec(),
        warm_idle: None,
        released_idle: None,
    };
    st.say(format!(
        "begin; pid {}; light-probe={} exclude-from-capture={}",
        std::process::id(),
        a.opts.light_probe,
        a.cfg.general.exclude_from_capture && !a.opts.no_exclude
    ));
    a.selftest = Some(st);
    a.sched.set(T_SCRIPT, now + SCRIPT[0].0);
}

pub fn step(a: &mut App, now: f64) {
    let Some(mut st) = a.selftest.take() else {
        return;
    };
    if st.next >= SCRIPT.len() {
        a.selftest = Some(st);
        return;
    }
    let (due, act) = SCRIPT[st.next];
    st.next += 1;
    let late = now - (st.start + due);
    if late > 0.1 {
        st.say(format!(
            "timer for {act:?} fired {:.0} ms late (the loop was busy, blocked or the process stalled)",
            late * 1000.0
        ));
    }
    run(a, &mut st, act, now);
    if let Some(&(t, _)) = SCRIPT.get(st.next) {
        a.sched.set(T_SCRIPT, st.start + t);
    }
    a.selftest = Some(st);
}

fn run(a: &mut App, st: &mut SelfTest, act: Act, now: f64) {
    let (mon_cx, mon_top, ppd) = (
        a.layout.mon.rect.left + a.layout.mon.width() / 2,
        a.layout.mon.rect.top,
        a.layout.px_per_dip,
    );
    match act {
        Act::Baseline => {
            let m = sys::proc_metrics();
            let gpu = a.stage.as_ref().map(|s| {
                format!(
                    "'{}'{}",
                    s.gpu.adapter_name,
                    if s.gpu.is_warp { " (WARP)" } else { "" }
                )
            });
            match gpu {
                Some(g) => st.say(format!(
                    "gpu stack warm on {g}; created in {:.1} ms",
                    a.last_warm_ms
                )),
                None => st.fail("GPU stack could not be created at start-up".into()),
            }
            st.say(format!("start-up: private WS {:.1} MiB, working set {:.1} MiB, cpu {:.2}s, monitor {}x{} @ {} dpi ({:.2} px/DIP), refresh {:.2} ms", sys::mib(m.private_ws), sys::mib(m.working_set), m.cpu_secs, a.layout.mon.width(), a.layout.mon.height(), a.layout.mon.dpi, ppd, a.period * 1000.0));
        }
        Act::ProbePill => {
            let y = mon_top + (a.cfg.appearance.pill_height * ppd * 0.5).round() as i32;
            match probe_pixel(mon_cx, y) {
                Some((r, g, b)) => st.say(format!(
                    "probe idle pill at ({mon_cx},{y}): rgb({r},{g},{b}) expected {}",
                    if a.opts.light_probe {
                        "~(244,244,246)"
                    } else {
                        "black (0,0,0)"
                    }
                )),
                None => st.say("probe idle pill: GetPixel unavailable on this session".into()),
            }
        }
        Act::Expand => a.expand(Trigger::Hotkey),
        Act::ProbeExpanded => {
            let y = mon_top + (40.0 * ppd).round() as i32;
            match probe_pixel(mon_cx, y) {
                Some((r, g, b)) => {
                    let light_ok = r > 200 && g > 200 && b > 200;
                    let dark_ok = r < 30 && g < 30 && b < 30;
                    let ok = if a.opts.light_probe {
                        light_ok
                    } else {
                        dark_ok
                    };
                    st.say(format!("probe expanded notch at ({mon_cx},{y}): rgb({r},{g},{b}) -> {}", if ok { "matches the notch fill" } else { "does NOT match the notch fill (not composed on screen, or desktop probing unavailable)" }));
                }
                None => st.say("probe expanded: GetPixel unavailable on this session".into()),
            }
        }
        Act::Scroll => {
            if a.shell.scroll(now, 0.0, -120.0).is_some() {
                a.kick("switch");
            }
        }
        Act::Collapse => a.collapse(true),
        Act::IdleBegin | Act::IdleBeginReleased => st.idle_begin = Some(mark()),
        Act::IdleEndWarm | Act::IdleEndReleased => {
            let end = mark();
            if let Some(b) = st.idle_begin.take() {
                let secs = (end.t - b.t).max(1e-3);
                // Exact cycle count when available; the scheduler-tick figure is printed beside it.
                let tick_cpu = (end.m.cpu_secs - b.m.cpu_secs) / secs * 100.0;
                let cpu = if st.cycles_hz > 1e6 && end.m.cycles > 0 {
                    end.m.cycles.saturating_sub(b.m.cycles) as f64 / st.cycles_hz / secs * 100.0
                } else {
                    tick_cpu
                };
                let mib = sys::mib(end.m.private_ws);
                let warm = matches!(act, Act::IdleEndWarm);
                st.say(format!("idle ({}) over {secs:.1}s: cpu {cpu:.4}% (scheduler ticks: {tick_cpu:.2}%)  private WS {mib:.1} MiB  working set {:.1} MiB  commit {:.1} MiB", if warm { "GPU warm" } else { "GPU released" }, sys::mib(end.m.working_set), sys::mib(end.m.private_commit)));
                if warm {
                    st.warm_idle = Some((cpu, mib))
                } else {
                    st.released_idle = Some((cpu, mib))
                }
            }
        }
        Act::Release => {
            a.release_gpu();
            if a.stage.is_some() {
                st.fail("GPU stack did not release".into());
            }
        }
        Act::Warm => {
            let ok = a.warm_gpu();
            if !ok {
                st.fail("GPU stack could not be re-created after release".into());
            } else {
                st.say(format!(
                    "warm from released state took {:.1} ms",
                    a.last_warm_ms
                ));
            }
        }
        Act::ExpandHover => a.expand(Trigger::Hover),
        Act::Peek => {
            a.shell.collapse(now);
            if let Some((owner, size)) = a
                .host
                .page_ids()
                .into_iter()
                .find_map(|id| Some((a.host.peek_owner(id)?, a.host.peek_size(id)?)))
            {
                a.shell.peek(now, owner, size, 1.5);
                a.kick("peek");
            }
        }
        Act::MediaFeed => {
            // A synthetic session goes through the real pipeline: bus -> module -> display list ->
            // image cache -> Direct2D bitmap -> composition.
            let (r, g, b) = ART_RGB;
            let px = [b, g, r, 255].repeat(64 * 64);
            let id = ImageData::new(64, 64, px)
                .and_then(|d| a.images.put(d))
                .map_or(0, |i| i.0);
            if id == 0 {
                st.fail("the image cache refused the demo art".into());
            }
            st.art_id = id;
            a.bus_tx.send(
                Source::Local,
                EventKind::MediaChanged(Arc::new(demo_snapshot("Selftest Track A", id))),
            );
        }
        Act::MediaExpand => {
            let ring = a.host.page_ids();
            st.say(format!("pages after the media event: {ring:?}"));
            match a.host.page_of("media") {
                Some(p) => {
                    a.shell.set_page(now, p);
                    a.expand(Trigger::Hotkey);
                }
                None => st
                    .fail("the media page did not join the ring after a MediaChanged event".into()),
            }
        }
        Act::ProbeMedia => {
            if let (Some(p), true) = (
                a.host.page_of("media"),
                a.shell.presence() == Presence::Expanded,
            ) {
                let size = a.pages[p];
                let area = compose::content_rect(0.0, size, a.layout.win_dip.w, &a.metrics);
                let c = MediaLayout::new(area).art.center();
                let (wx, wy, _, _) = a.layout.win_px;
                let (x, y) = (
                    wx + (c.x * ppd).round() as i32,
                    wy + (c.y * ppd).round() as i32,
                );
                match probe_pixel(x, y) {
                    Some((r, g, b)) => {
                        let (er, eg, eb) = ART_RGB;
                        let close = |v: u8, e: u8| v.abs_diff(e) <= 6;
                        let ok = close(r, er) && close(g, eg) && close(b, eb);
                        st.say(format!(
                            "probe album art at ({x},{y}): rgb({r},{g},{b}) expected ~({er},{eg},{eb}) -> {}",
                            if ok { "matches: the image reached the screen" } else { "does NOT match (art not drawn, or desktop probing unavailable)" }
                        ));
                    }
                    None => st.say("probe album art: GetPixel unavailable on this session".into()),
                }
                st.say(format!(
                    "image cache: {} image(s), {} KiB; GPU bitmaps uploaded: {}",
                    a.images.len(),
                    a.images.bytes() / 1024,
                    a.stage.as_ref().map_or(0, |s| s.images.uploaded())
                ));
            } else {
                st.fail("the media page was not expanded for the art probe".into());
            }
        }
        Act::MediaNext => {
            // A different track while collapsed must announce itself with a peek.
            a.bus_tx.send(
                Source::Local,
                EventKind::MediaChanged(Arc::new(demo_snapshot("Selftest Track B", st.art_id))),
            );
        }
        Act::CheckPeek => {
            if a.shell.presence() == Presence::Peek {
                st.say("a track change while collapsed showed the peek banner".into());
            } else {
                st.fail(format!(
                    "a track change did not peek (presence {:?})",
                    a.shell.presence()
                ));
            }
        }
        Act::MediaGone => {
            a.bus_tx.send(
                Source::Local,
                EventKind::MediaChanged(Arc::new(MediaSnapshot::default())),
            );
            a.images.remove(notch_core::draw::ImageId(st.art_id));
        }
        Act::CheckGone => {
            let ring = a.host.page_ids();
            if ring.contains(&"media") {
                st.fail(format!(
                    "the media page stayed after the session ended: {ring:?}"
                ));
            } else {
                st.say(format!(
                    "session ended: page left the ring {ring:?}; cache holds {} image(s)",
                    a.images.len()
                ));
            }
            st.say(format!(
                "audio meter thread running: {} (it must only run while the visualizer is on screen)",
                a.services.audio.running()
            ));
            if a.services.audio.running() {
                st.fail("the audio meter thread is running while nothing is animating".into());
            }
        }
        Act::ClipText => {
            st.clip_mark = clip_events(a);
            if !textclip::copy_text(a.ctrl, CLIP_TEXT) {
                st.fail("could not put text on the clipboard".into());
            }
        }
        Act::ClipTextCheck => match new_clip_item(a, st) {
            Some(it) if it.kind == ClipKind::Text && it.preview.as_ref() == CLIP_TEXT => {
                st.clip_text_id = it.id;
                st.say(format!(
                    "clipboard: text copied by 'another app' arrived as entry {}",
                    it.id
                ));
            }
            Some(it) => st.fail(format!(
                "clipboard: unexpected entry {:?} '{}'",
                it.kind, it.preview
            )),
            None => st.fail("clipboard: a copied text never reached the bus".into()),
        },
        Act::ClipLink => {
            st.clip_mark = clip_events(a);
            if !textclip::copy_text(a.ctrl, CLIP_LINK) {
                st.fail("could not put a link on the clipboard".into());
            }
        }
        Act::ClipLinkCheck => match new_clip_item(a, st) {
            Some(it) if it.kind == ClipKind::Link => {
                st.say(format!(
                    "clipboard: link recognised, previewed as '{}'",
                    it.preview
                ));
            }
            Some(it) => st.fail(format!("clipboard: a link was classified as {:?}", it.kind)),
            None => st.fail("clipboard: a copied link never reached the bus".into()),
        },
        Act::ClipImage => {
            st.clip_mark = clip_events(a);
            let (r, g, b) = CLIP_RGB;
            let px = [b, g, r, 255].repeat(64 * 64);
            if !clip::put_dib(a.ctrl, 64, 64, &px) {
                st.fail("could not put a bitmap on the clipboard".into());
            }
        }
        Act::ClipImageCheck => match new_clip_item(a, st) {
            Some(it) if it.kind == ClipKind::Image && it.thumb != 0 => {
                let thumb_ok = a.images.contains(notch_core::draw::ImageId(it.thumb));
                let blobs = std::fs::read_dir(paths::data_dir().join("clips"))
                    .map(|d| {
                        d.filter_map(Result::ok)
                            .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
                            .count()
                    })
                    .unwrap_or(0);
                st.say(format!(
                    "clipboard: bitmap arrived as '{}' (thumbnail cached: {thumb_ok}; full-size PNG files on disk: {blobs})",
                    it.preview
                ));
                if !thumb_ok || blobs == 0 {
                    st.fail("clipboard: the image thumbnail or its stored PNG is missing".into());
                }
            }
            Some(it) => st.fail(format!(
                "clipboard: bitmap arrived as {:?} thumb {}",
                it.kind, it.thumb
            )),
            None => st.fail(
                "clipboard: a copied bitmap never reached the bus (DIB -> BMP -> WIC path)".into(),
            ),
        },
        Act::ClipExcluded => {
            st.clip_mark = clip_events(a);
            if !clip::put_excluded_text(a.ctrl, "secret-password-123") {
                st.fail("could not put the excluded text on the clipboard".into());
            }
        }
        Act::ClipExcludedCheck => {
            if new_clip_item(a, st).is_some() {
                st.fail("clipboard: content marked 'exclude from monitoring' WAS recorded".into());
            } else {
                st.say("clipboard: content flagged 'exclude from monitoring' (password managers) was ignored".into());
            }
        }
        Act::ClipRecopy => {
            st.clip_mark = clip_events(a);
            a.services
                .command(&Command::Clipboard(ClipCmd::Copy(st.clip_text_id)));
        }
        Act::ClipRecopyCheck => {
            let text = clip::current_text(a.ctrl);
            let moved = new_clip_item(a, st).map(|i| i.id);
            if text.as_deref() == Some(CLIP_TEXT) && moved == Some(st.clip_text_id) {
                st.say("clipboard: clicking an entry put its text back and moved it to the front, without recording a duplicate".into());
            } else {
                st.fail(format!(
                    "clipboard: re-copy failed (clipboard text {text:?}, moved entry {moved:?})"
                ));
            }
        }
        Act::ClipPin => {
            a.services
                .command(&Command::Clipboard(ClipCmd::Pin(st.clip_text_id, true)));
        }
        Act::ClipPinCheck => match std::fs::read_to_string(pins_file()) {
            Ok(t) if t.contains(CLIP_TEXT) => {
                st.say("clipboard: a pinned text was saved to pins.json".into())
            }
            other => st.fail(format!(
                "clipboard: pins.json missing or wrong after pinning: {other:?}"
            )),
        },
        Act::ClipUnpin => {
            a.services
                .command(&Command::Clipboard(ClipCmd::Pin(st.clip_text_id, false)));
        }
        Act::ClipUnpinCheck => {
            if pins_file().exists() {
                st.fail("clipboard: pins.json still exists after unpinning the last pin".into());
            } else {
                st.say("clipboard: unpinning the last pin removed pins.json (nothing of the history stays on disk)".into());
            }
        }
        Act::ClipPage => match a.host.page_of("clipboard") {
            Some(p) => {
                a.shell.set_page(now, p);
                a.expand(Trigger::Hotkey);
            }
            None => st.fail("the clipboard page is not in the ring".into()),
        },
        Act::ClipProbe => {
            if let Some(p) = a.host.page_of("clipboard") {
                // Newest first: [re-copied text, image, link] -> the image is row 1.
                let (x, y) = clip_thumb_pixel(a, p, 1);
                match probe_pixel(x, y) {
                    Some((r, g, b)) => {
                        let (er, eg, eb) = CLIP_RGB;
                        let close = |v: u8, e: u8| v.abs_diff(e) <= 8;
                        let ok = close(r, er) && close(g, eg) && close(b, eb);
                        st.say(format!(
                            "probe clipboard image thumbnail at ({x},{y}): rgb({r},{g},{b}) expected ~({er},{eg},{eb}) -> {}",
                            if ok { "matches: thumbnail drawn in the list" } else { "does NOT match (row layout changed, or desktop probing unavailable)" }
                        ));
                    }
                    None => st.say(
                        "probe clipboard thumbnail: GetPixel unavailable on this session".into(),
                    ),
                }
            }
            a.collapse(true);
        }
        Act::ShelfDrop => {
            let dir =
                std::env::temp_dir().join(format!("shark-notch-selftest-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let (f1, f2) = (dir.join("report.txt"), dir.join("notes.md"));
            let _ = std::fs::write(&f1, vec![b'x'; 1234]);
            let _ = std::fs::write(&f2, b"# notes\n");
            st.shelf_dir = Some(dir);
            st.shelf_mark = (
                a.bus_counts[Kind::DragHover as usize],
                a.bus_counts[Kind::FileDropped as usize],
            );
            let paths = [
                f1.to_string_lossy().into_owned(),
                f2.to_string_lossy().into_owned(),
            ];
            match dragdrop::data_object_for(&paths) {
                None => st.fail(
                    "shelf: the shell could not build a data object for two real files".into(),
                ),
                Some(data) => match simulate_drop(a, &data) {
                    Some([enter, over, drop]) if enter == 1 && over == 1 && drop == 1 => {
                        st.say("shelf: the OLE drop target accepted a file drag (enter, over, drop all answered COPY)".into());
                    }
                    other => st.fail(format!(
                        "shelf: the drop target answered {other:?} (expected COPY=1 three times)"
                    )),
                },
            }
        }
        Act::ShelfCheck => {
            let hovers = a.bus_counts[Kind::DragHover as usize] - st.shelf_mark.0;
            let drops = a.bus_counts[Kind::FileDropped as usize] - st.shelf_mark.1;
            match (&a.last_files, drops) {
                (Some(files), d) if d >= 1 && files.len() == 2 => {
                    let names: Vec<String> = files.iter().map(|f| f.name.to_string()).collect();
                    let big = files.iter().find(|f| f.name.as_ref() == "report.txt");
                    let thumbs = files.iter().filter(|f| f.thumb != 0).count();
                    st.say(format!(
                        "shelf: drop arrived as {names:?}; report.txt is {} bytes; {thumbs} of 2 have a shell thumbnail/icon; {hovers} drag-hover event(s)",
                        big.map_or(0, |f| f.size)
                    ));
                    if big.map(|f| f.size) != Some(1234) {
                        st.fail("shelf: the dropped file's size is wrong".into());
                    }
                    if thumbs == 0 {
                        st.fail(
                            "shelf: no dropped file got an icon or thumbnail from the shell".into(),
                        );
                    }
                    if hovers < 2 {
                        st.fail("shelf: the drag highlight events (hover on, hover off) did not both arrive".into());
                    }
                }
                _ => st.fail(format!(
                    "shelf: the dropped files never reached the bus ({drops} event(s))"
                )),
            }
        }
        Act::ShelfPage => match a.host.page_of("shelf") {
            Some(p) => {
                a.shell.set_page(now, p);
                a.expand(Trigger::Hotkey);
            }
            None => st.fail("the shelf page is not in the ring".into()),
        },
        Act::ShelfProbe => {
            let sources = a.list.hits.iter().filter(|h| h.draggable).count();
            st.say(format!(
                "shelf: the open page registered {sources} drag source(s) ({} hit regions in all)",
                a.list.hits.len()
            ));
            if sources < 2 {
                st.fail("shelf: the dropped files did not become draggable tiles".into());
            }
            a.collapse(true);
        }
        Act::ShelfOff => {
            // With the shelf switched off nothing may be accepted: the module and its worker are gone.
            let mut cfg = a.cfg.clone();
            cfg.shelf.enabled = false;
            a.services.sync(&cfg);
            let paths = st
                .shelf_dir
                .as_ref()
                .map(|d| vec![d.join("report.txt").to_string_lossy().into_owned()])
                .unwrap_or_default();
            let verdict = dragdrop::data_object_for(&paths).and_then(|d| simulate_drop(a, &d));
            match verdict {
                Some([0, 0, 0]) => st.say(
                    "shelf: with the module switched off the drop target refuses drags".into(),
                ),
                other => st.fail(format!(
                    "shelf: a drag was not refused while the shelf is off: {other:?}"
                )),
            }
            // And back on, as the user left it.
            let cfg = a.cfg.clone();
            a.services.sync(&cfg);
        }
        Act::ShelfOffCheck => {
            let on = a.services.shelf().is_some();
            st.say(format!(
                "shelf: worker running again after re-enabling: {on}"
            ));
            if !on {
                st.fail("shelf: the worker did not come back after re-enabling the module".into());
            }
            if let Some(d) = st.shelf_dir.take() {
                let _ = std::fs::remove_dir_all(d);
            }
        }
        Act::NotifAccess => {
            a.collapse(true);
            // The exe must carry the manifest that lets a registered sparse package give it an identity.
            match embedded_manifest() {
                Some(m) if m.contains("<msix") && m.contains("packageName=\"SharkNotch\"") => st
                    .say("notifications: the exe embeds its application manifest with the sparse-package (<msix>) declaration".into()),
                Some(_) => st.fail("notifications: the embedded application manifest lacks the <msix> sparse-package declaration".into()),
                None => st.fail("notifications: the exe has no embedded application manifest (build.rs did not embed it)".into()),
            }
            match a.last_notif_access {
                Some(acc) => st.say(format!(
                    "notifications: the Windows listener reported {acc:?} (a plain unpackaged exe has no package identity, so NoIdentity is the expected answer here)"
                )),
                None => st.fail("notifications: the service never reported an access state".into()),
            }
        }
        Act::NotifFeed => {
            // A phone-sourced notification while collapsed: the banner must come up by itself.
            a.bus_tx.send(
                Source::Phone,
                demo_note(1, "Selftest ping", "Hello from the iPhone path"),
            );
        }
        Act::NotifCheckPeek => {
            let text = drawn_text(a);
            let titled = text.iter().any(|t| t == "Selftest ping");
            let tagged = text.iter().any(|t| t.contains("iPhone"));
            if a.shell.presence() == Presence::Peek && titled {
                st.say(format!(
                    "notifications: a new notification showed a banner with its title (device tag shown: {tagged})"
                ));
            } else {
                st.fail(format!(
                    "notifications: no banner for a new notification (presence {:?}, title drawn: {titled})",
                    a.shell.presence()
                ));
            }
            if !tagged {
                st.fail(
                    "notifications: a phone notification's banner does not say where it came from"
                        .into(),
                );
            }
        }
        Act::NotifAway => {
            // A game takes over the screen: the real suspension path (windows hidden, GPU released).
            a.shell.collapse(now);
            a.fs_active = true;
            a.apply_suspension();
            st.notif_mark = a.bus_counts[Kind::Notification as usize];
            a.bus_tx.send(
                Source::Phone,
                demo_note(2, "Selftest away 1", "arrived during the game"),
            );
            a.bus_tx.send(
                Source::Phone,
                demo_note(3, "Selftest away 2", "also during the game"),
            );
        }
        Act::NotifAwayCheck => {
            let got = a.bus_counts[Kind::Notification as usize] - st.notif_mark;
            let quiet = a.stage.is_none() && a.shell.presence() == Presence::Hidden;
            st.say(format!(
                "notifications: {got} arrived while a fullscreen app was in front; windows hidden and GPU released throughout: {quiet}; badge pending: {}",
                a.host.chips_width() > 0.0
            ));
            if got != 2 {
                st.fail(
                    "notifications: the notifications sent while away did not reach the module"
                        .into(),
                );
            }
            if !quiet {
                st.fail("notifications: something woke the notch (banner, GPU or window) while a fullscreen app was in front".into());
            }
            if a.host.chips_width() == 0.0 {
                st.fail(
                    "notifications: nothing counted the notifications missed while away".into(),
                );
            }
        }
        Act::NotifBack => {
            a.fs_active = false;
            a.apply_suspension();
        }
        Act::NotifBackCheck => {
            let text = drawn_text(a);
            let said = text.iter().any(|t| t == "While you were away")
                && text.iter().any(|t| t == "2 notifications");
            st.say(format!(
                "notifications: back from the game, presence {:?}; the summary banner says what was missed: {said}",
                a.shell.presence()
            ));
            st.frames_mark = a.frames_presented;
            if a.shell.presence() != Presence::Peek || !said {
                st.fail("notifications: no \"while you were away\" summary after returning from fullscreen".into());
            }
        }
        Act::NotifChipCheck => {
            let (presence, text) = (a.shell.presence(), drawn_text(a));
            let frame = a.shell.frame();
            a.maybe_release_gpu(now);
            let kept = a.stage.is_some();
            st.say(format!(
                "notifications: summary tucked away (presence {presence:?}); the missed-count badge is drawn: {}; GPU kept while the badge shows: {kept}",
                text.iter().any(|t| t == "2")
            ));
            st.say(format!(
                "notifications: {} frame(s) were presented since the summary banner; pill {:.0}x{:.0} DIP, chip alpha {:.2}, content {:?}, animating {}",
                a.frames_presented - st.frames_mark,
                frame.shape.w,
                frame.shape.h,
                frame.chip_alpha,
                frame.content_kind,
                a.animating()
            ));
            if presence != Presence::Collapsed {
                st.fail("notifications: the summary banner did not tuck itself away".into());
            }
            if !kept {
                st.fail(
                    "notifications: the GPU stack was released while the missed badge was showing"
                        .into(),
                );
            }
            if !text.iter().any(|t| t == "2") {
                st.fail("notifications: the collapsed pill does not show the missed count".into());
            }
        }
        Act::NotifOpen => match a.host.page_of("notifications") {
            Some(p) => {
                a.shell.set_page(now, p);
                a.expand(Trigger::Hotkey);
            }
            None => st.fail("the notifications page is not in the ring".into()),
        },
        Act::NotifOpenCheck => {
            let text = drawn_text(a);
            let rows = ["Selftest ping", "Selftest away 1", "Selftest away 2"]
                .iter()
                .filter(|r| text.iter().any(|t| t == **r))
                .count();
            let cleared = a.host.chips_width() == 0.0;
            st.say(format!(
                "notifications: the open page lists {rows} of 3 notifications; looking at the page cleared the badge: {cleared}"
            ));
            if rows != 3 {
                st.fail("notifications: the page does not list every notification".into());
            }
            if !cleared {
                st.fail("notifications: the missed badge stayed after the page was opened".into());
            }
        }
        Act::NotifClose => a.collapse(true),
        Act::NotifReleaseCheck => {
            a.maybe_release_gpu(now);
            let released = a.stage.is_none();
            st.say(format!(
                "notifications: with the badge gone the GPU releases as usual: {released}"
            ));
            if !released {
                st.fail(
                    "notifications: the GPU stack was not released after the badge went away"
                        .into(),
                );
            }
        }
        Act::CalSetup => {
            // A real feed file: the whole chain (service -> time zone -> bus -> module) is exercised.
            let now = unix_now();
            let today = sys::local_time();
            let ics = format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
                 BEGIN:VEVENT\r\nUID:st-1\r\nDTSTART:{}\r\nDURATION:PT20M\r\nSUMMARY:Selftest meeting\r\nLOCATION:https://meet.google.com/abc-defg-hij\r\nEND:VEVENT\r\n\
                 BEGIN:VEVENT\r\nUID:st-2\r\nDTSTART:{}\r\nDURATION:PT30M\r\nRRULE:FREQ=DAILY;COUNT=5\r\nSUMMARY:Selftest daily\r\nEND:VEVENT\r\n\
                 BEGIN:VEVENT\r\nUID:st-3\r\nDTSTART;VALUE=DATE:{:04}{:02}{:02}\r\nSUMMARY:Selftest all-day\r\nEND:VEVENT\r\n\
                 END:VCALENDAR\r\n",
                ics_stamp(now + 150),
                ics_stamp(now + 3 * 3600),
                today.year,
                today.month,
                today.day
            );
            let path = paths::data_dir().join("selftest.ics");
            let _ = std::fs::write(&path, ics);
            st.cal_mark = a.bus_counts[Kind::Calendar as usize];
            let feed = path.to_string_lossy().into_owned();
            reconfigure(a, |c| c.calendar.feeds = vec![feed]);
        }
        Act::CalCheck => {
            let got = a.bus_counts[Kind::Calendar as usize] - st.cal_mark;
            let text = drawn_text(a);
            let banner = a.shell.presence() == Presence::Peek
                && text.iter().any(|t| t == "Selftest meeting");
            let join = a.list.hits.iter().any(|h| h.id.0 == 300);
            let chip = a.host.chips_width() > 0.0;
            st.say(format!(
                "calendar: {got} refresh(es) from the feed file; banner for the meeting (5 min lead): {banner}; Join button in it: {join}; chip on the pill: {chip}"
            ));
            if got == 0 {
                st.fail("calendar: the service never published the feed".into());
            }
            if !banner || !join {
                st.fail(
                    "calendar: no banner with a Join button for a meeting 2 minutes away".into(),
                );
            }
            if !chip {
                st.fail("calendar: no countdown chip for a meeting starting soon".into());
            }
        }
        Act::CalJoin => {
            if !click_region(a, 300) {
                st.fail("calendar: the banner has no Join region to click".into());
            }
        }
        Act::CalJoinCheck => {
            let opened = a.last_open_url.take();
            st.say(format!("calendar: clicking Join asked to open {opened:?}"));
            if opened.as_deref() != Some("https://meet.google.com/abc-defg-hij") {
                st.fail("calendar: Join did not open the meeting link".into());
            }
        }
        Act::CalPage => match a.host.page_of("calendar") {
            Some(p) => {
                a.shell.set_page(now, p);
                a.expand(Trigger::Hotkey);
            }
            None => st.fail("the calendar page is not in the ring".into()),
        },
        Act::CalPageCheck => {
            let text = drawn_text(a);
            let t = sys::local_time();
            let month = format!(
                "{} {}",
                notch_core::civil::MONTHS[t.month as usize - 1],
                t.year
            );
            let has = |s: &str| text.iter().any(|x| x == s);
            let joins = a
                .list
                .hits
                .iter()
                .filter(|h| (200..210).contains(&h.id.0))
                .count();
            let cells = a
                .list
                .hits
                .iter()
                .filter(|h| (100..142).contains(&h.id.0))
                .count();
            st.say(format!(
                "calendar: page shows '{month}': {}; agenda lists the meeting: {}, the daily series: {}, the all-day event: {}; {cells} day cells, {joins} Join button(s)",
                has(&month),
                has("Selftest meeting"),
                has("Selftest daily"),
                has("Selftest all-day")
            ));
            if !has(&month) || !has("Selftest meeting") || !has("Selftest all-day") {
                st.fail("calendar: the page does not show the month and today's events".into());
            }
            if cells != 42 || joins == 0 {
                st.fail("calendar: the grid or the Join button is missing".into());
            }
        }
        Act::CalClose => a.collapse(true),
        Act::CalChipCheck => {
            let text = drawn_text(a);
            let chip = text.iter().find(|t| {
                *t == "now"
                    || t.strip_suffix('m')
                        .is_some_and(|n| n.parse::<u32>().is_ok())
            });
            a.maybe_release_gpu(now);
            st.say(format!(
                "calendar: the collapsed pill shows a countdown chip: {chip:?}; GPU kept for it: {}",
                a.stage.is_some()
            ));
            if chip.is_none() {
                st.fail("calendar: the collapsed pill shows no countdown".into());
            }
            if a.stage.is_none() {
                st.fail("calendar: the GPU stack was released while a chip is on the pill".into());
            }
        }
        Act::CalOff => {
            reconfigure(a, |c| c.calendar.feeds.clear());
            let _ = std::fs::remove_file(paths::data_dir().join("selftest.ics"));
        }
        Act::PomoSetup => reconfigure(a, |c| {
            c.pomodoro.focus_minutes = 0.05;
            c.pomodoro.short_break_minutes = 0.05;
            c.pomodoro.auto_start_breaks = true;
            c.pomodoro.sound = false;
        }),
        Act::PomoPage => match a.host.page_of("pomodoro") {
            Some(p) => {
                a.shell.set_page(now, p);
                a.expand(Trigger::Hotkey);
            }
            None => st.fail("the pomodoro page is not in the ring".into()),
        },
        Act::PomoStart => {
            // Play, through the real mouse path; then look at the pill.
            if !click_region(a, 1) {
                st.fail("pomodoro: the page has no play button".into());
            }
            a.collapse(true);
        }
        Act::PomoRunCheck => {
            let text = drawn_text(a);
            let chip = text.iter().any(|t| t == "1m");
            st.say(format!(
                "pomodoro: a running session shows a chip with the minutes left: {chip}; pill chips width {:.0} DIP",
                a.host.chips_width()
            ));
            if !chip || a.host.chips_width() == 0.0 {
                st.fail("pomodoro: no chip while the timer runs".into());
            }
        }
        Act::PomoEndCheck => {
            let text = drawn_text(a);
            let banner = text.iter().any(|t| t == "Focus session complete");
            st.say(format!(
                "pomodoro: the session ended by itself (3 s in this test): the banner says so: {banner} (presence {:?})",
                a.shell.presence()
            ));
            if !banner {
                st.fail("pomodoro: no banner when the focus session ended".into());
            }
        }
        Act::PomoAddOpen => match a.host.page_of("pomodoro") {
            Some(p) => {
                a.shell.set_page(now, p);
                a.expand(Trigger::Hotkey);
            }
            None => st.fail("the pomodoro page is not in the ring".into()),
        },
        Act::PomoAdd => {
            // Click "Add task", then type through the same handlers the window messages use.
            if !click_region(a, 4) {
                st.fail("pomodoro: the page has no Add task field".into());
            }
            let asked = a.kbd_owner == Some("pomodoro");
            if !asked {
                st.fail("pomodoro: clicking Add task did not ask for the keyboard".into());
            }
            for c in "Ship the selftest ".encode_utf16() {
                a.on_char(u32::from(c));
            }
            // A character outside the BMP arrives as two surrogates.
            for u in "🦈".encode_utf16() {
                a.on_char(u32::from(u));
            }
            let enter = a.on_keydown(0x0D);
            st.say(format!(
                "pomodoro: Add task took the keyboard ({asked}); Enter handled ({enter}); keyboard given back: {}",
                a.kbd_owner.is_none()
            ));
            if a.kbd_owner.is_some() {
                st.fail("pomodoro: the keyboard was not given back after Enter".into());
            }
        }
        Act::PomoAddCheck => {
            let text = drawn_text(a);
            let listed = text.iter().any(|t| t == "Ship the selftest 🦈");
            st.say(format!("pomodoro: the typed task is in the list: {listed}"));
            if !listed {
                st.fail(format!("pomodoro: the typed task is not listed: {text:?}"));
            }
        }
        Act::PomoSaveCheck => {
            let path = paths::data_dir().join("pomodoro.json");
            match std::fs::read_to_string(&path) {
                Ok(json) if json.contains("Ship the selftest") => st.say(format!(
                    "pomodoro: the task list was saved to pomodoro.json ({} bytes) a moment after the change",
                    json.len()
                )),
                Ok(_) => st.fail("pomodoro: pomodoro.json does not contain the task".into()),
                Err(e) => st.fail(format!("pomodoro: nothing was saved ({e})")),
            }
        }
        Act::PomoOff => {
            a.collapse(true);
            reconfigure(a, |c| c.pomodoro.enabled = false);
            reconfigure(a, |c| c.pomodoro.enabled = true);
        }
        Act::LivePage => match a.host.page_of("live") {
            Some(p) => {
                a.shell.set_page(now, p);
                a.expand(Trigger::Hotkey);
            }
            None => st.fail("the live page is not in the ring".into()),
        },
        Act::LiveClick => {
            // The first quick-timer button (1 minute), through the real mouse path.
            if !click_region(a, 100) {
                st.fail("live: the page has no quick-timer button".into());
            }
        }
        Act::LiveClickCheck => {
            let text = drawn_text(a);
            let row = text.iter().any(|t| t == "1 min");
            let clock = text
                .iter()
                .any(|t| t == "01:00" || t == "00:59" || t == "00:58");
            st.say(format!(
                "live: a quick timer started from the page: row '1 min' listed: {row}; its countdown is drawn: {clock}"
            ));
            if !row || !clock {
                st.fail(format!("live: the started timer is not listed: {text:?}"));
            }
        }
        Act::LiveCancel => {
            if !click_region(a, 200) {
                st.fail("live: the timer row has no cancel button".into());
            }
        }
        Act::LiveCancelCheck => {
            let text = drawn_text(a);
            let gone = !text.iter().any(|t| t == "1 min");
            st.say(format!("live: cancelling removes the timer: {gone}"));
            if !gone {
                st.fail("live: the timer is still listed after cancelling".into());
            }
        }
        Act::LiveClose => a.collapse(true),
        Act::LiveTimerLoad => {
            // A timer that ends in 3 s, handed over the way a saved one is (the store's answer):
            // this exercises restore, the module's own wake-up, the banner and the chime path.
            let end = unix_now() + 3;
            let json = format!(
                "{{\"items\":[{{\"id\":7,\"label\":\"x\",\"end\":{end},\"total\":60}}],\"next_id\":8}}"
            );
            a.bus_tx.send(
                Source::Local,
                EventKind::StoreLoaded(notch_core::events::StoreItem {
                    key: "timers".into(),
                    data: Some(json.into()),
                }),
            );
        }
        Act::LiveTimerChipCheck => {
            let text = drawn_text(a);
            let chip = text.iter().any(|t| t == "1m");
            a.maybe_release_gpu(now);
            st.say(format!(
                "live: a running timer shows a chip on the collapsed pill: {chip}; GPU kept for it: {}",
                a.stage.is_some()
            ));
            if !chip || a.host.chips_width() == 0.0 {
                st.fail("live: no chip for a running timer".into());
            }
            if a.stage.is_none() {
                st.fail("live: the GPU stack was released while a timer chip is showing".into());
            }
        }
        Act::LiveTimerEndCheck => {
            let text = drawn_text(a);
            let banner =
                a.shell.presence() == Presence::Peek && text.iter().any(|t| t == "Timer finished");
            st.say(format!(
                "live: the timer ended by itself (3 s): the banner says so: {banner}; its chip is gone: {}",
                !text.iter().any(|t| t == "1m")
            ));
            if !banner {
                st.fail(format!("live: no banner when the timer ended: {text:?}"));
            }
        }
        Act::LiveTimerClose => a.collapse(true),
        Act::PrivacyOn => {
            if !a.opts.registry_probe {
                st.say("live: microphone chip skipped (it writes a fake usage record to the registry; pass --registry-probe to run it)".into());
            } else {
                privacy::probe::clear();
                if let Err(e) = privacy::probe::set_usage(privacy::probe::now(), 0) {
                    st.fail(format!("live: {e}"));
                }
            }
        }
        Act::PrivacyCheck => {
            if a.opts.registry_probe {
                let text = drawn_text(a);
                let named = text.iter().any(|t| t == privacy::probe::FAKE_NAME);
                a.maybe_release_gpu(now);
                st.say(format!(
                    "live: a program using the microphone (a fake record in Windows' consent store) shows on the pill by name: {named}; GPU kept for it: {}",
                    a.stage.is_some()
                ));
                if !named || a.host.chips_width() == 0.0 {
                    st.fail(format!(
                        "live: no privacy chip for a program on the microphone: {text:?}"
                    ));
                }
                if a.stage.is_none() {
                    st.fail(
                        "live: the GPU stack was released while the privacy chip is showing".into(),
                    );
                }
            }
        }
        Act::PrivacyOff => {
            if a.opts.registry_probe {
                privacy::probe::clear();
            }
        }
        Act::PrivacyOffCheck => {
            if a.opts.registry_probe {
                a.maybe_release_gpu(now);
                st.say(format!(
                    "live: the record was removed: the chip is gone ({}) and the GPU released ({})",
                    a.host.chips_width() == 0.0,
                    a.stage.is_none()
                ));
                if a.host.chips_width() != 0.0 {
                    st.fail(
                        "live: the privacy chip stayed after the microphone was released".into(),
                    );
                }
                if a.stage.is_some() {
                    st.fail(
                        "live: the GPU stack was not released after the privacy chip went".into(),
                    );
                }
            }
        }
        Act::DlSetup => {
            // Watch a scratch folder (never the real Downloads folder), write a browser-style
            // partial file into it.
            let dir = paths::data_dir().join("Downloads");
            let _ = std::fs::create_dir_all(&dir);
            st.dl_mark = (
                a.bus_counts[Kind::Downloads as usize],
                a.bus_counts[Kind::DownloadDone as usize],
            );
            let setting = dir.to_string_lossy().into_owned();
            reconfigure(a, |c| c.live.download_dir = setting);
            if let Err(e) = std::fs::write(dir.join("video.mp4.crdownload"), vec![0x5au8; 1 << 20])
            {
                st.fail(format!("live: cannot write the test download ({e})"));
            }
            st.dl_dir = Some(dir);
        }
        Act::DlCheck => {
            let got = a.bus_counts[Kind::Downloads as usize] - st.dl_mark.0;
            let text = drawn_text(a);
            let chip = text.iter().find(|t| t.contains("MB")).cloned();
            st.say(format!(
                "live: a partial file in the watched folder became a download: {got} list update(s); chip text {chip:?}"
            ));
            if got == 0 || chip.is_none() || a.host.chips_width() == 0.0 {
                st.fail(format!(
                    "live: the download did not reach the pill: {text:?}"
                ));
            }
        }
        Act::DlGrow => {
            if let Some(dir) = &st.dl_dir
                && let Err(e) =
                    std::fs::write(dir.join("video.mp4.crdownload"), vec![0x5au8; 6 << 20])
            {
                st.fail(format!("live: cannot grow the test download ({e})"));
            }
        }
        Act::DlGrowCheck => {
            let text = drawn_text(a);
            let chip = text.iter().find(|t| t.contains("MB")).cloned();
            st.say(format!(
                "live: the download grew to 6 MiB; the chip follows: {chip:?}"
            ));
            let follows = chip
                .as_deref()
                .is_some_and(|c| c.starts_with("6.0 MB") || c.ends_with("/s"));
            if !follows {
                st.fail(format!(
                    "live: the chip did not follow the growing download: {text:?}"
                ));
            }
        }
        Act::DlFinish => {
            if let Some(dir) = &st.dl_dir
                && let Err(e) =
                    std::fs::rename(dir.join("video.mp4.crdownload"), dir.join("video.mp4"))
            {
                st.fail(format!("live: cannot finish the test download ({e})"));
            }
        }
        Act::DlFinishCheck => {
            let done = a.bus_counts[Kind::DownloadDone as usize] - st.dl_mark.1;
            let text = drawn_text(a);
            let has = |s: &str| text.iter().any(|t| t == s);
            let banner = a.shell.presence() == Presence::Peek
                && has("Download complete")
                && has("video.mp4")
                && has("Show");
            st.say(format!(
                "live: the browser's rename to the real name is a finished download: {done} completion event(s); banner with the file name, its size ({}) and a Show button: {banner}",
                has("6.0 MB")
            ));
            if done != 1 || !banner || !has("6.0 MB") {
                st.fail(format!(
                    "live: no proper banner for the finished download: {text:?}"
                ));
            }
        }
        Act::DlShow => {
            if !click_region(a, 900) {
                st.fail("live: the download banner has no Show region to click".into());
            }
        }
        Act::DlShowCheck => {
            let want = st
                .dl_dir
                .as_ref()
                .map(|d| d.join("video.mp4").to_string_lossy().into_owned());
            let got = a.last_reveal.take();
            st.say(format!(
                "live: clicking Show asked Explorer to reveal {got:?} (recorded, not executed, in the self-test)"
            ));
            if want.as_deref() != got.as_deref() {
                st.fail(format!("live: Show revealed {got:?}, expected {want:?}"));
            }
        }
        Act::DlOff => {
            a.collapse(true);
            reconfigure(a, |c| c.live.download_dir = String::new());
            if let Some(dir) = st.dl_dir.take() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
        Act::StatsPage => match a.host.page_of("stats") {
            Some(p) => {
                a.shell.set_page(now, p);
                a.expand(Trigger::Hotkey);
            }
            None => st.fail("the stats page is not in the ring".into()),
        },
        Act::StatsBegin => {
            st.stats_mark = a.bus_counts[Kind::Stats as usize];
            st.stats_begin = Some(mark());
        }
        Act::StatsCheck => {
            let end = mark();
            let got = a.bus_counts[Kind::Stats as usize] - st.stats_mark;
            let text = drawn_text(a);
            let has = |s: &str| text.iter().any(|t| t == s);
            let percent = text
                .iter()
                .filter(|t| t.ends_with('%') && t.len() <= 4)
                .count();
            let charts = a
                .list
                .cmds
                .iter()
                .filter(|c| matches!(c, DrawCmd::Shape { .. }))
                .count();
            st.say(format!(
                "stats: {got} reading(s) in the 3.2 s the page was open (one per second asked for by the host's poll); tiles CPU/Memory/GPU/Network/Battery drawn: {}; {percent} percentage value(s); {charts} chart shape(s) in the last frame",
                has("CPU") && has("Memory") && has("GPU") && has("Network") && has("Battery")
            ));
            if let Some(b) = st.stats_begin.take() {
                let secs = (end.t - b.t).max(1e-3);
                let cpu = if st.cycles_hz > 1e6 && end.m.cycles > 0 {
                    end.m.cycles.saturating_sub(b.m.cycles) as f64 / st.cycles_hz / secs * 100.0
                } else {
                    (end.m.cpu_secs - b.m.cpu_secs) / secs * 100.0
                };
                st.say(format!(
                    "stats: with the page open and sampling once a second this process used {cpu:.2}% of a core over {secs:.1} s ({:.1} MiB private)",
                    sys::mib(end.m.private_ws)
                ));
            }
            for (label, ok) in [
                ("a few readings arrived while the page was open", got >= 2),
                (
                    "the tiles are drawn",
                    has("CPU") && has("Memory") && has("GPU") && has("Network") && has("Battery"),
                ),
                ("CPU and memory show percentages", percent >= 2),
                ("history charts are drawn", charts >= 2),
            ] {
                if !ok {
                    st.fail(format!("stats: {label}: no ({text:?})"));
                }
            }
        }
        Act::StatsClose => a.collapse(true),
        Act::StatsSettled => st.stats_mark = a.bus_counts[Kind::Stats as usize],
        Act::StatsQuietCheck => {
            let extra = a.bus_counts[Kind::Stats as usize] - st.stats_mark;
            st.say(format!(
                "stats: {extra} reading(s) in the 2.6 s after the page was closed (nothing may be measured while it is not on screen)"
            ));
            if extra != 0 {
                st.fail("stats: readings kept arriving after the page was closed".into());
            }
        }
        Act::Report => finish(a, st),
    }
}

fn finish(a: &mut App, st: &mut SelfTest) {
    if a.present_errors > 0 {
        st.fail(format!(
            "{} frame(s) failed to draw/present",
            a.present_errors
        ));
    }
    if a.frames_presented < 30 {
        st.fail(format!("only {} frames were presented", a.frames_presented));
    }
    let mut report = String::new();
    let _ = writeln!(
        report,
        "frame-time bursts (hitch = interval > 1.5x refresh period):"
    );
    let _ = writeln!(report, "{}", a.recorder.format_all());
    let _ = writeln!(
        report,
        "frames presented: {}  errors: {}  total hitches: {}",
        a.frames_presented,
        a.present_errors,
        a.recorder.total_hitches()
    );
    if let (Some(w), Some(r)) = (st.warm_idle, st.released_idle) {
        let _ = writeln!(
            report,
            "memory: private working set {:.1} MiB (GPU warm) -> {:.1} MiB (GPU released); idle CPU {:.4}% / {:.4}%",
            w.1, r.1, w.0, r.0
        );
    }
    st.say(report.trim_end().to_string());
    let hb = st.heartbeat.report(st.start);
    st.say(hb);
    st.say(format!(
        "result: {}",
        if st.failures.is_empty() {
            "PASS".to_string()
        } else {
            format!("FAIL ({})", st.failures.join("; "))
        }
    ));

    let text: String = st.lines.iter().map(|l| format!("SELFTEST {l}\n")).collect();
    if let Some(path) = &a.opts.out {
        let _ = std::fs::write(path, &text);
    }
    st.exit_code = if st.failures.is_empty() { 0 } else { 1 };
    st.finished = true;
}
