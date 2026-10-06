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

use std::sync::Arc;

use notch_core::compose;
use notch_core::events::{EventKind, MediaSnapshot, Source};
use notch_core::image::ImageData;
use notch_core::modules::media::Layout as MediaLayout;
use notch_core::shell::{Presence, Trigger};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};

use crate::app::{App, T_SCRIPT};
use crate::win::clock;
use crate::win::sys::{self, ProcMetrics};

#[derive(Clone, Copy)]
struct Mark {
    t: f64,
    m: ProcMetrics,
}

pub struct SelfTest {
    start: f64,
    next: usize,
    pub finished: bool,
    pub exit_code: i32,
    lines: Vec<String>,
    failures: Vec<String>,
    idle_begin: Option<Mark>,
    /// Id of the synthetic album art in the image cache.
    art_id: u64,
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
    (31.5, Act::Report),
];

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

pub fn begin(a: &mut App) {
    let now = clock::now();
    if a.opts.light_probe {
        a.cfg.appearance.theme = notch_core::theme::ThemeMode::Light;
        a.refresh_theme();
    }
    let mut st = SelfTest {
        start: now,
        next: 0,
        finished: false,
        exit_code: 0,
        lines: Vec::new(),
        failures: Vec::new(),
        idle_begin: None,
        art_id: 0,
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
    let (_, act) = SCRIPT[st.next];
    st.next += 1;
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
