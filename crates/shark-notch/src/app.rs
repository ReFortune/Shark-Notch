//! The controller: owns all state, translates OS events into inputs for the pure core, and runs the
//! single UI loop.
//!
//! The loop sleeps in `MsgWaitForMultipleObjectsEx` with an `INFINITE` timeout unless something is
//! pending. The swap chain's frame-latency handle is added to the wait set **only while animating**,
//! so an idle notch costs no wake-ups beyond the 10 Hz cursor sample.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use notch_core::bus::{Bus, BusSender, Waker};
use notch_core::color::Color;
use notch_core::compose::{self, Metrics};
use notch_core::config::{Config, FullscreenScope, Loaded, ReduceMotion, Style};
use notch_core::draw::{CursorKind, DrawList, HitId};
use notch_core::events::{EventKind, Kind, Source};
use notch_core::frame::{self, FrameRecorder};
use notch_core::fullscreen::IRect;
use notch_core::geom::{Rect, Size, Vec2};
use notch_core::hover::{Cadence, HoverAction, HoverFsm, HoverParams};
use notch_core::image::ImageCache;
use notch_core::input::Input;
use notch_core::module::{Audio, Command, Env, ModuleHost, ShelfCmd, ShellRequest};
use notch_core::modules;
use notch_core::raster;
use notch_core::sched::{Scheduler, TimerId};
use notch_core::shell::{Presence, Shell, ShellConfig, ShellFrame, Trigger};
use notch_core::theme::Theme;
use std::sync::Arc;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, DispatchMessageW, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_SYSTEM_FOREGROUND, HTCLIENT, HTTRANSPARENT, IDC_ARROW, IDC_HAND, IDC_IBEAM, LoadCursorW,
    MA_NOACTIVATE, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW,
    PostQuitMessage, QS_ALLINPUT, RegisterWindowMessageW, SW_SHOWNORMAL, SetCursor,
    TranslateMessage, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_DWMCOLORIZATIONCOLORCHANGED,
    WM_ENDSESSION, WM_ERASEBKGND, WM_HOTKEY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCHITTEST, WM_PAINT, WM_QUIT, WM_RBUTTONUP,
    WM_SETCURSOR, WM_SETTINGCHANGE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::gfx::pill::PillWindow;
use crate::gfx::stack::GpuStack;
use crate::gfx::stage::{Stage, is_device_lost};
use crate::services::Services;
use crate::win::autostart;
use crate::win::cfgwatch::ConfigWatch;
use crate::win::clock;
use crate::win::dragdrop;
use crate::win::fullscreen::{self, Watcher};
use crate::win::hotkeys::{self, Hotkeys};
use crate::win::inbox::{self, Msg, WM_APP_WAKE};
use crate::win::layout::Layout;
use crate::win::paths;
use crate::win::sampler::Sampler;
use crate::win::session::{self, SessionWatch};
use crate::win::single::{CONTROLLER_CLASS, WM_SECOND_INSTANCE};
use crate::win::sys;
use crate::win::textclip;
use crate::win::tray::{MenuCmd, MenuState, Tray, WM_TRAY};
use crate::win::util::{wide, x_of, y_of};
use crate::win::window;

// ----- timers ---------------------------------------------------------------------------------

pub const T_HOVER: TimerId = TimerId(1);
pub const T_GPU_RELEASE: TimerId = TimerId(2);
pub const T_FS_RECHECK: TimerId = TimerId(3);
pub const T_CFG_RELOAD: TimerId = TimerId(4);
pub const T_MODULES: TimerId = TimerId(5);
pub const T_REDRAW: TimerId = TimerId(6);
pub const T_SCRIPT: TimerId = TimerId(9);

/// A continuous animation (the visualizer) reports its frame times every this many frames.
const LONG_BURST_FRAMES: usize = 1200;

/// `WM_MOUSELEAVE` (declared in commctrl.h, outside the feature set we link).
const WM_MOUSELEAVE: u32 = 0x02A3;

#[derive(Clone, Default)]
pub struct Options {
    pub selftest: bool,
    pub out: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub no_exclude: bool,
    pub light_probe: bool,
    pub console: bool,
}

/// Wakes the UI thread when a worker pushes an event onto the bus.
struct WinWaker;

impl Waker for WinWaker {
    fn wake(&self) {
        inbox::wake();
    }
}

pub struct App {
    pub(crate) ctrl: HWND,
    pub(crate) opts: Options,
    pub(crate) cfg: Config,
    pub(crate) config_path: PathBuf,
    pub(crate) theme: Theme,
    pub(crate) system_dark: bool,
    pub(crate) system_accent: Color,
    pub(crate) shell: Shell,
    pub(crate) hover: HoverFsm,
    pub(crate) sched: Scheduler,
    pub(crate) list: DrawList,
    pub(crate) recorder: FrameRecorder,
    pub(crate) pages: Vec<Size>,
    pub(crate) host: ModuleHost,
    bus: Bus,
    pub(crate) bus_tx: BusSender,
    /// Decoded album art / thumbnails, shared with the producers and the renderer.
    pub(crate) images: Arc<ImageCache>,
    /// OS-backed producers (media session, audio meter, ...), alive only while their module is.
    pub(crate) services: Services,
    /// Bus events delivered so far, per kind, and the last clipboard item (self-test evidence).
    pub(crate) bus_counts: [u32; Kind::ALL.len()],
    pub(crate) last_clip: Option<notch_core::events::ClipboardItem>,
    pub(crate) last_files: Option<Vec<notch_core::events::FileEntry>>,
    /// The notch's OLE drop target (registered on every stage window while the shelf is active).
    pub(crate) drop_target: Option<windows::Win32::System::Ole::IDropTarget>,
    /// Files the shelf asked to drag out; OLE's modal loop runs from the main loop, outside any borrow.
    pending_drag: Option<Vec<Arc<str>>>,
    press_pos: Option<Vec2>,
    /// The last view reported to the host: (expanded, ring page).
    view: (bool, usize),
    system_24h: bool,
    pub(crate) metrics: Metrics,
    pub(crate) layout: Layout,
    pub(crate) stage: Option<Stage>,
    pub(crate) pill: Option<PillWindow>,
    sampler: Sampler,
    watcher: Watcher,
    pub(crate) fs_active: bool,
    pub(crate) paused: bool,
    locked: bool,
    display_off: bool,
    tray: Option<Tray>,
    tray_hidden: bool,
    hotkeys: Hotkeys,
    _session: Option<SessionWatch>,
    cfg_watch: Option<ConfigWatch>,
    last_interactive: bool,
    burst: bool,
    pub(crate) period: f64,
    cursor: Vec2,
    press: Option<HitId>,
    tracking_leave: bool,
    status: String,
    taskbar_created: u32,
    pub(crate) selftest: Option<crate::diag::SelfTest>,
    pub(crate) frames_presented: u64,
    pub(crate) present_errors: u64,
    pub(crate) last_warm_ms: f64,
    pub(crate) last_prewarm_ms: f64,
    slow_frames_logged: u32,
    frame_block_until: f64,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` with the app if it is not already borrowed (window messages can re-enter synchronously).
pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut g| g.as_mut().map(f))
    })
}

fn guard<R: Default>(f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        error!("panic caught at a window-procedure boundary");
        R::default()
    })
}

// ----- config -> core ---------------------------------------------------------------------------

fn reduce_motion(cfg: &Config) -> bool {
    match cfg.animation.reduce_motion {
        ReduceMotion::On => true,
        ReduceMotion::Off => false,
        ReduceMotion::System => !sys::animations_enabled(),
    }
}

fn shell_config(cfg: &Config) -> ShellConfig {
    let (a, an) = (&cfg.appearance, &cfg.animation);
    ShellConfig {
        collapsed: Size::new(a.pill_width, a.pill_height),
        chip_height: 30.0,
        radius_collapsed: a.corner_radius * 0.4,
        radius_expanded: a.corner_radius,
        ear_collapsed: if a.ears { a.ear_size * 0.35 } else { 0.0 },
        ear_expanded: if a.ears { a.ear_size } else { 0.0 },
        smoothing: a.corner_smoothing,
        floating_gap: if a.style == Style::Island { 8.0 } else { 0.0 },
        idle_visible: a.idle_visible,
        speed: an.speed,
        bounciness: an.bounciness,
        stagger_in: an.stagger_in_ms as f64 / 1000.0,
        stagger_out: an.stagger_out_ms as f64 / 1000.0,
        stagger_switch: 0.045,
        reduce_motion: reduce_motion(cfg),
    }
}

/// Content metrics: how far content must stay clear of the shape's edges and ears.
fn make_metrics(cfg: &Config, theme: &Theme) -> Metrics {
    let ears = cfg.appearance.ears && cfg.appearance.style != Style::Island;
    Metrics {
        ear_inset: if ears { cfg.appearance.ear_size } else { 0.0 },
        outline: theme.dark,
        ..Metrics::default()
    }
}

fn hover_params(cfg: &Config) -> HoverParams {
    let h = &cfg.hover;
    HoverParams {
        zone_half_width: h.zone_width * 0.5,
        zone_height: h.zone_height,
        approach_margin: h.approach_margin,
        dwell: h.dwell_ms as f64 / 1000.0,
        leave_grace: h.leave_grace_ms as f64 / 1000.0,
        typing_suppress: h.typing_suppress_ms as f64 / 1000.0,
        ..HoverParams::default()
    }
}

fn resolve_theme(cfg: &Config, system_dark: bool, system_accent: Color) -> Theme {
    let accent = if cfg.appearance.accent == "system" {
        system_accent
    } else {
        Color::from_hex(&cfg.appearance.accent).unwrap_or(system_accent)
    };
    Theme::resolve(cfg.appearance.theme, system_dark, accent)
}

/// Module page sizes limited to the fixed panel the window was sized for.
fn clamp_pages(cfg: &Config, pages: Vec<Size>) -> Vec<Size> {
    let max = Size::new(
        cfg.appearance.max_panel_width,
        cfg.appearance.max_panel_height,
    );
    pages
        .into_iter()
        .map(|p| Size::new(p.w.min(max.w), p.h.min(max.h)))
        .collect()
}

fn make_layout(cfg: &Config) -> Option<Layout> {
    let mon = sys::select_monitor(&cfg.general.monitor)?;
    Some(Layout::new(
        mon,
        cfg.appearance.scale,
        Size::new(
            cfg.appearance.max_panel_width,
            cfg.appearance.max_panel_height,
        ),
        Size::new(cfg.appearance.pill_width, cfg.appearance.pill_height),
    ))
}

fn dwm_timing() -> Option<(f64, f64)> {
    let mut info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info) }.ok()?;
    let period = clock::ticks_to_duration_secs(info.qpcRefreshPeriod);
    (period > 0.0).then(|| (clock::qpc_to_secs(info.qpcVBlank as i64), period))
}

// ----- the app ----------------------------------------------------------------------------------

impl App {
    fn new(ctrl: HWND, opts: Options) -> Result<App, String> {
        let config_path = opts.config.clone().unwrap_or_else(paths::config_path);
        let mut warnings = Vec::new();
        let cfg = match std::fs::read_to_string(&config_path) {
            Ok(text) => match Config::parse(&text) {
                Ok(l) => {
                    warnings = l.warnings;
                    l.config
                }
                Err(e) => {
                    warnings.push(format!("config error: {e}"));
                    Config::default()
                }
            },
            Err(_) => {
                // First run: write the commented defaults so there is something to edit.
                if let Some(dir) = config_path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&config_path, notch_core::config::DEFAULT_TOML);
                Config::default()
            }
        };
        crate::log::set_level(crate::log::Level::parse(&cfg.general.log_level));
        for w in &warnings {
            warn!("config: {w}");
        }

        let system_dark = sys::system_dark();
        let system_accent = sys::system_accent();
        let theme = resolve_theme(&cfg, system_dark, system_accent);
        let host = ModuleHost::new(modules::registry(), Arc::new(cfg.clone()), theme);
        let pages = clamp_pages(&cfg, host.pages());
        let layout = make_layout(&cfg).ok_or("no monitor available")?;
        let (bus, bus_tx) = Bus::new(Arc::new(WinWaker));
        let images = Arc::new(ImageCache::default());
        let services = Services::new(bus_tx.clone(), images.clone());
        let mut shell = Shell::new(shell_config(&cfg));
        shell.set_pages(pages.clone());
        shell.gesture_mut().reverse = cfg.animation.reverse_scroll;
        let hover = HoverFsm::new(hover_params(&cfg));

        let mut app = App {
            ctrl,
            opts,
            config_path,
            theme,
            system_dark,
            system_accent,
            shell,
            hover,
            sched: Scheduler::new(),
            list: DrawList::new(),
            recorder: FrameRecorder::new(),
            pages,
            host,
            bus,
            bus_tx,
            images,
            services,
            bus_counts: [0; Kind::ALL.len()],
            last_clip: None,
            last_files: None,
            drop_target: None,
            pending_drag: None,
            press_pos: None,
            view: (false, 0),
            system_24h: sys::system_24h(),
            metrics: Metrics::default(),
            layout,
            stage: None,
            pill: None,
            sampler: Sampler::new(),
            watcher: Watcher::default(),
            fs_active: false,
            paused: false,
            locked: false,
            display_off: false,
            tray: None,
            tray_hidden: !cfg.general.tray_icon,
            hotkeys: Hotkeys::default(),
            _session: None,
            cfg_watch: None,
            last_interactive: false,
            burst: false,
            period: 1.0 / 60.0,
            cursor: Vec2::new(-1.0e4, -1.0e4),
            press: None,
            tracking_leave: false,
            status: warnings.first().cloned().unwrap_or_default(),
            taskbar_created: unsafe {
                RegisterWindowMessageW(PCWSTR(wide("TaskbarCreated").as_ptr()))
            },
            selftest: None,
            frames_presented: 0,
            present_errors: 0,
            last_warm_ms: 0.0,
            last_prewarm_ms: 0.0,
            slow_frames_logged: 0,
            frame_block_until: 0.0,
            cfg,
        };
        app.metrics = make_metrics(&app.cfg, &app.theme);
        Ok(app)
    }

    // ----- lifecycle --------------------------------------------------------------------------

    fn start(&mut self) {
        let now = clock::now();
        // The CPU pill first: the notch is visible immediately and costs almost nothing.
        match PillWindow::create(
            pill_proc,
            self.cfg.general.exclude_from_capture && !self.opts.no_exclude,
        ) {
            Ok(p) => self.pill = Some(p),
            Err(e) => error!("pill window: {e}"),
        }
        self.update_pill();

        let problems = self.hotkeys.apply(self.ctrl, &self.cfg.hotkeys);
        if let Some(p) = problems.first() {
            self.status = p.clone();
        }
        for p in problems {
            warn!("{p}");
        }
        self._session = Some(SessionWatch::register(self.ctrl));
        self.cfg_watch = ConfigWatch::new(
            self.config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        );
        if self.cfg.fullscreen.enabled {
            self.watcher.install();
            self.watcher
                .track(unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() });
        }
        if !self.tray_hidden && !self.opts.selftest {
            self.show_tray();
        }
        if self.cfg.general.autostart != autostart::is_enabled()
            && !self.opts.selftest
            && let Err(e) = autostart::set(self.cfg.general.autostart)
        {
            warn!("autostart: {e}");
        }

        self.services.sync(&self.cfg);
        self.drop_target = Some(dragdrop::new_target(
            self.bus_tx.clone(),
            self.services.shelf_slot(),
        ));

        // Pre-warm the first render so the very first hover animates immediately, then let the idle
        // timer release the GPU again if it is not used.
        if self.warm_gpu() {
            self.schedule_gpu_release(now);
        }
        self.recheck_fullscreen();
        self.start_sampling();
        info!(
            "started: monitor {} ({}x{} @ {} dpi), {} px/DIP",
            self.layout.mon.device,
            self.layout.mon.width(),
            self.layout.mon.height(),
            self.layout.mon.dpi,
            self.layout.px_per_dip
        );
    }

    fn show_tray(&mut self) {
        if self.tray.is_none() {
            self.tray = Tray::new(self.ctrl);
        }
        let tip = self.tooltip();
        if let Some(t) = self.tray.as_mut() {
            t.add(&tip);
        }
        self.tray_hidden = false;
    }

    fn tooltip(&self) -> String {
        if self.status.is_empty() {
            "Shark Notch".to_string()
        } else {
            format!("Shark Notch - {}", self.status)
        }
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
        let tip = self.tooltip();
        if let Some(t) = &self.tray {
            t.set_tooltip(&tip);
        }
    }

    pub(crate) fn suspended(&self) -> bool {
        self.paused || self.locked || self.display_off || self.fs_active
    }

    fn sensing_active(&self) -> bool {
        self.cfg.hover.enabled && !self.suspended()
    }

    fn start_sampling(&mut self) {
        if self.sensing_active() {
            self.sched.set(T_HOVER, clock::now());
        } else {
            self.sched.cancel(T_HOVER);
        }
    }

    // ----- GPU residency ----------------------------------------------------------------------

    pub(crate) fn warm_gpu(&mut self) -> bool {
        if self.stage.is_some() {
            return true;
        }
        let t0 = clock::now();
        let gpu = match GpuStack::create() {
            Ok(g) => g,
            Err(e) => {
                error!("cannot create the GPU stack: {e}");
                return false;
            }
        };
        let (name, warp) = (gpu.adapter_name.clone(), gpu.is_warp);
        let exclude = self.cfg.general.exclude_from_capture && !self.opts.no_exclude;
        let mut stage = match Stage::create(
            gpu,
            stage_proc,
            self.layout.win_px,
            self.layout.px_per_dip,
            exclude,
            self.images.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                error!("cannot create the stage window: {e}");
                return false;
            }
        };
        if let Some(t) = &self.drop_target {
            dragdrop::register(stage.hwnd, t);
        }
        // Draw the current (collapsed) state first so the window never shows undefined pixels.
        let frame = self.shell.frame();
        let env = self.env();
        self.host.set_context(clock::now(), env);
        compose::compose(
            &frame,
            &self.pages,
            &self.theme,
            self.layout.win_dip.w,
            &self.metrics,
            &mut self.list,
            &mut self.host,
        );
        if let Err(e) = stage.draw(&self.list) {
            error!("first frame failed: {e}");
            return false;
        }
        self.prewarm(&mut stage);
        if frame.visible {
            stage.show(self.layout.win_px);
        }
        self.stage = Some(stage);
        self.last_interactive = false;
        if let Some(p) = self.pill.as_mut() {
            p.hide();
        }
        self.last_warm_ms = (clock::now() - t0) * 1000.0;
        info!(
            "GPU stack warm in {:.1} ms (of which pre-warm {:.1} ms) on '{}'{}",
            self.last_warm_ms,
            self.last_prewarm_ms,
            name,
            if warp {
                " (WARP software renderer)"
            } else {
                ""
            }
        );
        true
    }

    /// Render every page, peek banner and the chips once into the not-yet-presented back buffer, so
    /// the first real animation finds its text formats, layouts, icon geometry and layer resources
    /// already built instead of creating them mid-expansion ("pre-warm the first render").
    fn prewarm(&mut self, stage: &mut Stage) {
        let t0 = clock::now();
        let peeks: Vec<(u32, Size)> = self
            .host
            .module_ids()
            .into_iter()
            .filter_map(|id| Some((self.host.peek_owner(id)?, self.host.peek_size(id)?)))
            .collect();
        // A 2x2 image so the bitmap-brush path is exercised too; removed again right after.
        let warm_image =
            notch_core::image::ImageData::new(2, 2, vec![128; 16]).and_then(|d| self.images.put(d));
        let plan = compose::Warmup {
            image: warm_image,
            pages: &self.pages,
            peeks: &peeks,
            chips_width: self.host.chips_width(),
            theme: &self.theme,
            window_w: self.layout.win_dip.w,
            metrics: &self.metrics,
        };
        let base = self.shell.frame();
        let n = plan.run(&base, &mut self.list, &mut self.host, |l| {
            stage.render_only(l).is_ok()
        });
        if let Some(id) = warm_image {
            self.images.remove(id);
        }
        self.last_prewarm_ms = (clock::now() - t0) * 1000.0;
        debug!("pre-warmed {n} frames in {:.1} ms", self.last_prewarm_ms);
    }

    pub(crate) fn release_gpu(&mut self) {
        if self.stage.is_none() {
            return;
        }
        // Show the CPU pill *before* the stage disappears so there is no gap.
        self.update_pill();
        self.stage = None;
        sys::trim_working_set();
        let m = sys::proc_metrics();
        info!(
            "GPU stack released; private working set {:.1} MiB",
            sys::mib(m.private_ws)
        );
    }

    fn schedule_gpu_release(&mut self, now: f64) {
        let secs = self.cfg.performance.gpu_idle_release_secs;
        if secs > 0 {
            self.sched.set(T_GPU_RELEASE, now + secs as f64);
        } else {
            self.sched.cancel(T_GPU_RELEASE);
        }
    }

    fn maybe_release_gpu(&mut self, now: f64) {
        let idle = self.shell.presence() == Presence::Collapsed && !self.animating() && !self.burst;
        if idle {
            self.release_gpu()
        } else {
            self.schedule_gpu_release(now)
        }
    }

    /// Draw the collapsed pill with the CPU rasteriser and show it.
    pub(crate) fn update_pill(&mut self) {
        let Some(pill) = self.pill.as_mut() else {
            return;
        };
        let f = self.shell.frame();
        let ppd = self.layout.px_per_dip;
        let shape = f.shape;
        if !f.visible || shape.w < 1.0 || shape.h < 0.5 {
            pill.hide();
            return;
        }
        let (w, h) = (
            (shape.w * ppd).ceil() as i32 + 2,
            (shape.h * ppd).ceil() as i32 + 1,
        );
        let path = shape.to_path().scaled(ppd).translated(Vec2::new(1.0, 0.0));
        let cov = raster::fill_polygons(&path.flatten(0.05), w as usize, h as usize);
        let bgra = raster::to_premultiplied_bgra(&cov, self.theme.bg);
        let mon = &self.layout.mon;
        let x = mon.rect.left + (mon.width() - w) / 2;
        let y = mon.rect.top + (f.y * ppd).round() as i32;
        match pill.update(&bgra, w, h, x, y) {
            Ok(()) => {
                if !pill.shown {
                    pill.show();
                }
            }
            Err(e) => warn!("pill update: {e}"),
        }
    }

    fn on_device_lost(&mut self) {
        warn!("GPU device lost; rebuilding the stack");
        self.stage = None;
        self.update_pill();
        if self.warm_gpu() {
            self.render_frame();
        }
    }

    // ----- frames -----------------------------------------------------------------------------

    /// Ensure the GPU is warm, open a frame-time burst and draw the first frame *now*.
    pub(crate) fn kick(&mut self, label: &str) {
        let now = clock::now();
        self.sched.cancel(T_GPU_RELEASE);
        if !self.warm_gpu() {
            return;
        }
        if !self.burst {
            self.burst = true;
            self.recorder.begin(label, now, self.period);
        }
        self.render_frame();
    }

    pub(crate) fn render_frame(&mut self) {
        let t_start = clock::now();
        if self.stage.is_none() {
            return;
        }
        if let Some((vblank, period)) = dwm_timing() {
            self.period = period;
            let t = frame::sample_time(t_start, Some(vblank), period);
            self.shell.step(t);
        } else {
            self.shell.step(t_start);
        }
        self.sync_view();
        let frame = self.shell.frame();
        self.sync_window(&frame);
        let env = self.frame_env();
        self.host.set_context(t_start, env);
        compose::compose(
            &frame,
            &self.pages,
            &self.theme,
            self.layout.win_dip.w,
            &self.metrics,
            &mut self.list,
            &mut self.host,
        );
        let t_composed = clock::now();
        let result = self.stage.as_mut().map(|s| s.draw(&self.list));
        let mut times = None;
        match result {
            Some(Ok(t)) => {
                self.frames_presented += 1;
                times = Some(t);
            }
            Some(Err(e)) if is_device_lost(&e) => {
                self.on_device_lost();
                return;
            }
            Some(Err(e)) => {
                self.present_errors += 1;
                self.frame_block_until = clock::now() + 0.05;
                error!("frame failed: {e}");
            }
            None => {}
        }
        if self.burst {
            let total_ms = ((clock::now() - t_start) * 1000.0) as f32;
            self.recorder.frame(t_start, total_ms);
            // A frame that cost more than two refresh periods is a real hitch: say where the time
            // went (the first few per run; a flood would be noise).
            if let Some(t) = times
                && f64::from(total_ms) > 2.2 * self.period * 1000.0
                && self.slow_frames_logged < 30
            {
                self.slow_frames_logged += 1;
                warn!(
                    "slow frame {total_ms:.1} ms: update+compose {:.1}, render {:.1}, present {:.1}",
                    ((t_composed - t_start) * 1000.0),
                    t.render_ms,
                    t.present_ms
                );
            }
        }
        if !self.animating() {
            self.end_burst();
        } else if self.recorder.len() >= LONG_BURST_FRAMES && !self.shell.animating() {
            // A long continuous run (the visualizer): report it in chunks so a hitch is not averaged
            // away inside one huge burst.
            self.rotate_burst(t_start);
        }
    }

    fn rotate_burst(&mut self, now: f64) {
        if let Some(rep) = self.recorder.finish()
            && (self.cfg.performance.frame_report || rep.has_hitch())
        {
            if rep.has_hitch() {
                warn!("{}", rep.format())
            } else {
                info!("{}", rep.format())
            }
        }
        self.recorder.begin("continuous", now, self.period);
    }

    fn end_burst(&mut self) {
        let now = clock::now();
        if self.burst {
            self.burst = false;
            if let Some(rep) = self.recorder.finish()
                && (self.cfg.performance.frame_report || rep.has_hitch())
            {
                if rep.has_hitch() {
                    warn!("{}", rep.format())
                } else {
                    info!("{}", rep.format())
                }
            }
        }
        if !self.host.wants_frames() {
            self.services.audio.stop();
        }
        if let Some(stage) = self.stage.as_mut() {
            stage.renderer.trim();
            stage.gpu.trim();
        }
        if self.shell.presence() == Presence::Collapsed {
            self.schedule_gpu_release(now);
        }
    }

    /// Keep the window's click-through state, hit region and visibility in step with the shell.
    fn sync_window(&mut self, frame: &ShellFrame) {
        let Some(stage) = self.stage.as_mut() else {
            return;
        };
        if frame.visible && !stage.shown {
            stage.show(self.layout.win_px);
        } else if !frame.visible && stage.shown {
            stage.hide();
        }
        if frame.interactive != self.last_interactive {
            self.last_interactive = frame.interactive;
            stage.set_click_through(!frame.interactive);
            if frame.interactive {
                // Cover the whole destination so clicks work during the animation, plus a margin.
                let r = self
                    .shell
                    .target_rect(self.layout.win_dip.w)
                    .inflate(6.0, 6.0);
                let ppd = self.layout.px_per_dip;
                let (pw, ph) = (self.layout.win_px.2, self.layout.win_px.3);
                let rect = RECT {
                    left: ((r.x * ppd).floor() as i32).clamp(0, pw),
                    top: ((r.y.max(0.0) * ppd).floor() as i32).clamp(0, ph),
                    right: ((r.right() * ppd).ceil() as i32).clamp(0, pw),
                    bottom: ((r.bottom() * ppd).ceil() as i32).clamp(0, ph),
                };
                stage.set_hit_region(Some(rect));
            } else {
                stage.set_hit_region(None);
            }
        }
    }

    // ----- triggers ---------------------------------------------------------------------------

    pub(crate) fn expand(&mut self, trigger: Trigger) {
        if self.suspended() {
            return;
        }
        let now = clock::now();
        self.shell.expand(now, trigger);
        self.kick("expand");
    }

    pub(crate) fn collapse(&mut self, manual: bool) {
        let now = clock::now();
        self.shell.collapse(now);
        if manual {
            self.hover.notify_manual_collapse();
        }
        self.kick("collapse");
    }

    fn on_hotkey(&mut self, id: i32) {
        match id {
            hotkeys::ID_TOGGLE => {
                if self.suspended() {
                    return;
                }
                if self.shell.is_expanded() {
                    self.collapse(true)
                } else {
                    self.expand(Trigger::Hotkey)
                }
            }
            hotkeys::ID_PEEK => {
                if self.fs_active && !self.cfg.fullscreen.peek_over_fullscreen {
                    return;
                }
                if self.suspended() && !self.fs_active {
                    return;
                }
                self.peek_over_fullscreen_if_needed();
                let now = clock::now();
                let target = self
                    .host
                    .page_ids()
                    .into_iter()
                    .find_map(|id| Some((self.host.peek_owner(id)?, self.host.peek_size(id)?)));
                if let Some((owner, size)) = target {
                    self.shell.peek(now, owner, size, 2.5);
                    self.kick("peek");
                }
            }
            hotkeys::ID_NEXT | hotkeys::ID_PREV if self.shell.presence() == Presence::Expanded => {
                let n = self.shell.page_count().max(1);
                let cur = self.shell.page();
                let next = if id == hotkeys::ID_NEXT {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                self.shell.set_page(clock::now(), next);
                self.kick("switch");
            }
            _ => {}
        }
    }

    /// The optional "peek" hotkey may show the notch briefly over a borderless-fullscreen app.
    fn peek_over_fullscreen_if_needed(&mut self) {
        if self.fs_active {
            let now = clock::now();
            self.shell.resume_now(now);
            self.update_pill();
        }
    }

    fn on_wheel(&mut self, dx: f32, dy: f32) {
        let now = clock::now();
        // A module with its own scrolling (lists, calendars) gets the wheel first.
        if self.shell.presence() == Presence::Expanded {
            let hit = self.hit_at(self.cursor);
            self.host_ctx(now);
            let consumed = self.host.input(
                self.shell.page(),
                hit,
                &Input::Wheel {
                    pos: self.cursor,
                    dx,
                    dy,
                },
            );
            self.after_host();
            if consumed {
                return;
            }
        }
        if self.shell.scroll(now, dx, dy).is_some() {
            self.kick("switch");
        }
    }

    /// Forward a pointer event to the module that owns the visible page.
    fn module_input(&mut self, input: Input) {
        let hit = self.hit_at(self.cursor);
        self.module_input_hit(input, hit);
    }

    /// Like `module_input`, with an explicit region (a drag reports where the press began).
    fn module_input_hit(&mut self, input: Input, hit: Option<HitId>) {
        if self.shell.presence() != Presence::Expanded {
            return;
        }
        self.host_ctx(clock::now());
        self.host.input(self.shell.page(), hit, &input);
        self.after_host();
    }

    fn hit_at(&self, p: Vec2) -> Option<HitId> {
        self.list.hit_test(p).map(|h| h.id)
    }

    /// Take the drag-out the shelf asked for (the main loop runs it outside any borrow of the app).
    pub(crate) fn take_pending_drag(&mut self) -> Option<Vec<String>> {
        self.pending_drag
            .take()
            .map(|v| v.iter().map(|p| p.to_string()).collect())
    }

    /// Before OLE's modal drag loop: release our own mouse capture and let the mouse fall through
    /// the notch, so the drop can land on whatever window is underneath it.
    pub(crate) fn before_drag(&mut self) {
        unsafe {
            let _ = ReleaseCapture();
        }
        if let Some(stage) = self.stage.as_mut() {
            stage.set_click_through(true);
            stage.set_hit_region(None);
        }
        self.press = None;
        self.press_pos = None;
        self.hover.reset();
    }

    /// After the drag loop returned: resynchronise the window state and the pointer bookkeeping.
    pub(crate) fn after_drag(&mut self, accepted: bool) {
        debug!("shelf drag-out finished (accepted: {accepted})");
        self.press = None;
        self.press_pos = None;
        self.tracking_leave = false;
        self.last_interactive = false; // forces `sync_window` to reapply click-through and the hit region
        if self.stage.is_some() {
            self.render_frame();
        }
        self.start_sampling();
    }

    /// A press and release on the same region: tell the module that owns the page.
    fn on_click(&mut self, id: HitId) {
        debug!("click on region {}", id.0);
        self.module_input(Input::Click(self.cursor));
    }

    // ----- the module host ----------------------------------------------------------------------

    fn env(&self) -> Env {
        Env {
            local: sys::local_time(),
            system_24h: self.system_24h,
            audio: Audio::Idle,
        }
    }

    /// The environment for a frame about to be drawn: includes the output level, sampled **only**
    /// while a visible module asks for continuous frames (the meter thread runs only then too).
    fn frame_env(&mut self) -> Env {
        let mut env = self.env();
        if self.host.wants_frames() {
            self.services.audio.start();
            env.audio = self.services.audio.sample();
        }
        env
    }

    fn host_ctx(&mut self, now: f64) {
        let env = self.env();
        self.host.set_context(now, env);
    }

    /// True while frames are needed: the shell is moving, or a visible module animates continuously.
    pub(crate) fn animating(&self) -> bool {
        self.shell.animating()
            || (self.shell.presence() == Presence::Expanded && self.host.wants_frames())
    }

    /// Report what is on screen to the host so it can load/unload modules and gate polling.
    pub(crate) fn sync_view(&mut self) {
        let expanded = self.shell.presence() == Presence::Expanded;
        let key = (expanded, if expanded { self.shell.page() } else { 0 });
        if key == self.view {
            return;
        }
        self.view = key;
        self.host_ctx(clock::now());
        self.host.set_view(expanded.then_some(key.1));
        self.after_host();
    }

    /// Keep the shell's page list and chip width in step with the modules.
    fn refresh_pages(&mut self) {
        let pages = clamp_pages(&self.cfg, self.host.pages());
        if pages != self.pages {
            self.pages = pages.clone();
            self.shell.set_pages(pages);
        }
        self.shell
            .set_chip_width(clock::now(), self.host.chips_width());
    }

    /// Execute what the host accumulated: commands, shell requests, redraws, crashed modules.
    fn after_host(&mut self) {
        for id in self.host.take_poisoned() {
            error!("module '{id}' panicked and was disabled");
            self.set_status(format!("module '{id}' crashed and was disabled"));
        }
        let out = self.host.take_out();
        self.refresh_pages();
        for c in out.commands {
            self.exec(c);
        }
        let now = clock::now();
        for r in out.shell {
            if self.suspended() {
                break;
            }
            match r {
                ShellRequest::Peek { module, duration } => {
                    if let (Some(owner), Some(size)) =
                        (self.host.peek_owner(module), self.host.peek_size(module))
                    {
                        self.shell.peek(now, owner, size, duration);
                        self.kick("peek");
                    }
                }
                ShellRequest::Expand { module } => {
                    if let Some(p) = self.host.page_of(module) {
                        self.shell.set_page(now, p);
                        self.expand(Trigger::Attention);
                    }
                }
            }
        }
        if out.redraw {
            self.redraw_once();
        }
        if let Some(t) = out.redraw_at {
            self.sched.set_earliest(T_REDRAW, t);
        }
        match self.host.next_deadline() {
            Some(t) => self.sched.set(T_MODULES, t),
            None => self.sched.cancel(T_MODULES),
        }
    }

    /// Draw one frame (no animation loop) because something visible changed.
    fn redraw_once(&mut self) {
        if self.suspended() {
            return;
        }
        let nothing_to_show =
            self.shell.presence() == Presence::Collapsed && self.host.chips_width() == 0.0;
        if nothing_to_show {
            return;
        }
        if self.stage.is_none() {
            self.warm_gpu();
        }
        self.render_frame();
    }

    fn exec(&mut self, c: Command) {
        match c {
            Command::Shelf(ShelfCmd::DragOut(paths)) => {
                // OLE's drag loop is modal: run it from the main loop, not from inside a message handler.
                if !paths.is_empty() {
                    self.pending_drag = Some(paths);
                }
            }
            Command::Media(_) | Command::Clipboard(_) | Command::Shelf(_) => {
                self.services.command(&c);
            }
            Command::OpenUrl(url) => {
                let u = url.trim();
                if u.starts_with("https://") || u.starts_with("http://") {
                    let w = wide(u);
                    unsafe {
                        ShellExecuteW(
                            None,
                            windows::core::w!("open"),
                            PCWSTR(w.as_ptr()),
                            PCWSTR::null(),
                            PCWSTR::null(),
                            SW_SHOWNORMAL,
                        );
                    }
                } else {
                    warn!("refused to open a non-http(s) URL");
                }
            }
        }
    }

    /// Deliver queued bus events to the modules.
    fn pump_bus(&mut self) {
        let mut events = Vec::new();
        self.bus.drain(&mut events);
        if events.is_empty() {
            return;
        }
        for ev in &events {
            self.bus_counts[ev.kind.kind() as usize] += 1;
            match &ev.kind {
                EventKind::ClipboardItem(it) => self.last_clip = Some(it.clone()),
                EventKind::FileDropped(f) => self.last_files = Some(f.clone()),
                _ => {}
            }
        }
        self.host_ctx(clock::now());
        self.host.dispatch(events);
        self.after_host();
    }

    // ----- hover sampling ---------------------------------------------------------------------

    fn hover_tick(&mut self, now: f64) {
        if !self.sensing_active() {
            return;
        }
        let presence = self.shell.presence();
        let expanded = presence == Presence::Expanded;
        let keep_open = if expanded || presence == Presence::Peek {
            let r = self
                .shell
                .target_rect(self.layout.win_dip.w)
                .inflate(8.0, 8.0);
            Some(Rect::new(r.x - self.layout.win_dip.w * 0.5, r.y, r.w, r.h))
        } else {
            None
        };
        let input = self.sampler.sample(now, &self.layout, keep_open);
        if presence == Presence::Peek {
            let inside = keep_open.is_some_and(|k| input.cursor.is_some_and(|c| k.contains(c)));
            self.shell.set_pointer_inside(now, inside);
        }
        let (action, cadence) = self.hover.update(&input, expanded);
        match action {
            HoverAction::None => {}
            HoverAction::Approach => {
                if self.cfg.performance.prewarm_on_approach && self.stage.is_none() {
                    self.warm_gpu();
                }
            }
            HoverAction::Expand => self.expand(Trigger::Hover),
            HoverAction::Collapse => {
                if !self.shell.is_sticky() {
                    self.collapse(false);
                }
            }
            HoverAction::ArmDrag => {
                debug!("a drag toward the top of the screen was detected");
                self.warm_gpu();
                if self.cfg.shelf.open_on_drag
                    && self.cfg.module_active("shelf")
                    && !self.suspended()
                    && let Some(p) = self.host.page_of("shelf")
                {
                    self.shell.set_page(now, p);
                    self.expand(Trigger::Drag);
                }
            }
            HoverAction::DisarmDrag => {}
        }
        let hz = self.cfg.hover.poll_hz.max(2) as f64;
        let delay = match cadence {
            Cadence::Idle => 1.0 / hz,
            Cadence::Near => 1.0 / 30.0,
            Cadence::Dwell => 1.0 / 60.0,
        };
        self.sched.set(T_HOVER, now + delay);
    }

    // ----- suspension (fullscreen, lock, display off, pause) -----------------------------------

    pub(crate) fn apply_suspension(&mut self) {
        let now = clock::now();
        let was_suspended_event = self.suspended();
        self.bus_tx
            .send(Source::Local, EventKind::Suspended(was_suspended_event));
        self.host_ctx(now);
        self.services.suspend(was_suspended_event);
        if was_suspended_event {
            self.host.suspend();
        } else {
            self.host.resume();
        }
        if self.suspended() {
            self.hover.reset();
            self.sched.cancel(T_HOVER);
            // Fullscreen/lock/display-off hide instantly: never start a GPU frame as a game takes over.
            let instant = self.fs_active || self.locked || self.display_off;
            if instant {
                self.shell.suspend_now(now);
                self.burst = false;
                let _ = self.recorder.finish();
                if let Some(s) = self.stage.as_mut() {
                    s.hide();
                }
                self.stage = None;
                if let Some(p) = self.pill.as_mut() {
                    p.hide();
                }
                self.sched.cancel(T_GPU_RELEASE);
                sys::trim_working_set();
                info!(
                    "suspended (fullscreen={}, locked={}, display_off={}); GPU released, windows hidden",
                    self.fs_active, self.locked, self.display_off
                );
            } else {
                self.shell.set_suspended(now, true);
                self.kick("pause");
            }
        } else {
            let was_instant_hidden =
                self.pill.as_ref().is_some_and(|p| !p.shown) && self.stage.is_none();
            if was_instant_hidden {
                self.shell.resume_now(now);
                self.update_pill();
            } else {
                self.shell.set_suspended(now, false);
                self.kick("resume");
            }
            self.start_sampling();
            info!("resumed");
        }
    }

    pub(crate) fn recheck_fullscreen(&mut self) {
        if !self.cfg.fullscreen.enabled {
            self.set_fullscreen(false);
            return;
        }
        let p = fullscreen::probe_foreground();
        let applies = p.fullscreen
            && match self.cfg.fullscreen.scope {
                FullscreenScope::Any => true,
                FullscreenScope::SameMonitor => {
                    let m = &self.layout.mon.rect;
                    p.monitor == Some(IRect::new(m.left, m.top, m.right, m.bottom))
                }
            };
        self.set_fullscreen(applies);
    }

    fn set_fullscreen(&mut self, on: bool) {
        if self.fs_active != on {
            self.fs_active = on;
            self.apply_suspension();
        }
    }

    // ----- window procedures' entry points ----------------------------------------------------

    fn on_stage_mouse(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) {
        match msg {
            WM_MOUSEMOVE => {
                self.cursor = self.layout.window_px_to_dip(x_of(lp.0), y_of(lp.0));
                // Button held since a press: a drag (the module gets the region the press began on).
                const MK_LBUTTON: usize = 0x0001;
                match self.press_pos {
                    Some(start) if wp.0 & MK_LBUTTON != 0 => {
                        let (pos, hit) = (self.cursor, self.press);
                        self.module_input_hit(Input::Drag { start, pos }, hit);
                    }
                    _ => self.module_input(Input::Move(self.cursor)),
                }
                if !self.tracking_leave
                    && let Some(stage) = &self.stage
                {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: stage.hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = unsafe { TrackMouseEvent(&mut tme) };
                    self.tracking_leave = true;
                }
            }
            WM_MOUSELEAVE => {
                self.tracking_leave = false;
                self.cursor = Vec2::new(-1.0e4, -1.0e4);
                self.module_input(Input::Leave);
            }
            WM_LBUTTONDOWN => {
                self.cursor = self.layout.window_px_to_dip(x_of(lp.0), y_of(lp.0));
                self.press = self.hit_at(self.cursor);
                self.press_pos = Some(self.cursor);
                self.module_input(Input::Down(self.cursor));
                if let Some(stage) = &self.stage {
                    unsafe {
                        SetCapture(stage.hwnd);
                    }
                }
            }
            WM_LBUTTONUP => {
                self.cursor = self.layout.window_px_to_dip(x_of(lp.0), y_of(lp.0));
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.module_input(Input::Up(self.cursor));
                self.press_pos = None;
                let released_on = self.hit_at(self.cursor);
                if let (Some(a), Some(b)) = (self.press.take(), released_on)
                    && a == b
                {
                    self.on_click(a);
                }
            }
            WM_MOUSEWHEEL => self.on_wheel(0.0, ((wp.0 >> 16) as u16 as i16) as f32),
            WM_MOUSEHWHEEL => self.on_wheel(((wp.0 >> 16) as u16 as i16) as f32, 0.0),
            _ => {}
        }
    }

    fn on_set_cursor(&self) -> bool {
        let kind = self
            .list
            .hit_test(self.cursor)
            .map(|h| h.cursor)
            .unwrap_or(CursorKind::Arrow);
        let id = match kind {
            CursorKind::Hand => IDC_HAND,
            CursorKind::IBeam => IDC_IBEAM,
            _ => IDC_ARROW,
        };
        unsafe {
            if let Ok(c) = LoadCursorW(None, id) {
                SetCursor(Some(c));
                return true;
            }
        }
        false
    }

    fn on_tray(&mut self, lp: LPARAM) {
        match (lp.0 as u32) & 0xFFFF {
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                let st = MenuState {
                    paused: self.paused,
                    autostart: autostart::is_enabled(),
                    hotkey: self.cfg.hotkeys.toggle.clone(),
                    status: self.status.clone(),
                };
                let cmd = self.tray.as_ref().and_then(|t| t.show_menu(&st));
                if let Some(cmd) = cmd {
                    self.on_menu(cmd);
                }
            }
            WM_LBUTTONUP | 0x400 => self.on_hotkey(hotkeys::ID_TOGGLE),
            _ => {}
        }
    }

    fn on_menu(&mut self, cmd: MenuCmd) {
        match cmd {
            MenuCmd::Toggle => self.on_hotkey(hotkeys::ID_TOGGLE),
            MenuCmd::Pause => {
                self.paused = !self.paused;
                self.apply_suspension();
            }
            MenuCmd::OpenConfig => {
                let path = wide(&self.config_path.to_string_lossy());
                unsafe {
                    ShellExecuteW(
                        None,
                        windows::core::w!("open"),
                        PCWSTR(path.as_ptr()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            MenuCmd::ReloadConfig => self.reload_config(),
            MenuCmd::Autostart => {
                let on = !autostart::is_enabled();
                if let Err(e) = autostart::set(on) {
                    self.set_status(format!("autostart failed: {e}"));
                }
            }
            MenuCmd::CopyFrames => {
                textclip::copy_text(self.ctrl, &self.recorder.format_all());
            }
            MenuCmd::CopyDiagnostics => {
                let text = self.diagnostics_text();
                textclip::copy_text(self.ctrl, &text);
            }
            MenuCmd::HideTray => {
                if let Some(t) = self.tray.as_mut() {
                    t.remove();
                }
                self.tray_hidden = true;
                info!("tray icon hidden (run the executable again to bring it back)");
            }
            MenuCmd::Quit => unsafe { PostQuitMessage(0) },
        }
    }

    pub(crate) fn diagnostics_text(&self) -> String {
        let m = sys::proc_metrics();
        let gpu = match &self.stage {
            Some(s) => format!(
                "warm on '{}'{}",
                s.gpu.adapter_name,
                if s.gpu.is_warp { " (WARP)" } else { "" }
            ),
            None => "released".into(),
        };
        format!(
            "Shark Notch diagnostics\nmonitor: {} {}x{} @ {} dpi ({:.2} px/DIP)\nrefresh period: {:.2} ms\ngpu: {}\nprivate working set: {:.1} MiB (working set {:.1} MiB, commit {:.1} MiB)\ncpu time so far: {:.2} s\nframes presented: {}  errors: {}\nsuspended: {} (fullscreen={} paused={} locked={} display_off={})\n\nframe-time bursts:\n{}\n\nlog tail:\n{}",
            self.layout.mon.device,
            self.layout.mon.width(),
            self.layout.mon.height(),
            self.layout.mon.dpi,
            self.layout.px_per_dip,
            self.period * 1000.0,
            gpu,
            sys::mib(m.private_ws),
            sys::mib(m.working_set),
            sys::mib(m.private_commit),
            m.cpu_secs,
            self.frames_presented,
            self.present_errors,
            self.suspended(),
            self.fs_active,
            self.paused,
            self.locked,
            self.display_off,
            self.recorder.format_all(),
            crate::log::recent(),
        )
    }

    fn on_session(&mut self, wp: usize) {
        let locked = match wp {
            session::WTS_SESSION_LOCK => true,
            session::WTS_SESSION_UNLOCK => false,
            _ => return,
        };
        if self.locked != locked {
            self.locked = locked;
            self.apply_suspension();
        }
    }

    fn on_display_state(&mut self, on: bool) {
        if self.display_off == on {
            self.display_off = !on;
            self.apply_suspension();
        }
    }

    fn on_display_change(&mut self) {
        let Some(new) = make_layout(&self.cfg) else {
            return;
        };
        let same = new.win_px == self.layout.win_px
            && (new.px_per_dip - self.layout.px_per_dip).abs() < 1e-3
            && new.mon.rect == self.layout.mon.rect;
        if same {
            return;
        }
        info!("display layout changed; rebuilding windows");
        self.layout = new;
        let had_stage = self.stage.is_some();
        self.stage = None;
        if had_stage && !self.suspended() {
            self.warm_gpu();
        }
        if !self.suspended() {
            self.update_pill();
        }
    }

    fn on_setting_change(&mut self, wp: usize, lp: isize) {
        let name = if lp != 0 {
            unsafe { PCWSTR(lp as *const u16).to_string().unwrap_or_default() }
        } else {
            String::new()
        };
        if name == "ImmersiveColorSet" {
            self.refresh_theme();
        }
        if name == "intl" {
            self.system_24h = sys::system_24h();
            self.redraw_once();
        }
        if wp == 0x1043 {
            // SPI_SETCLIENTAREAANIMATION
            let cfg = shell_config(&self.cfg);
            self.shell.set_config(cfg);
            info!(
                "animation effects changed; reduce motion is now {}",
                self.shell.config().reduce_motion
            );
        }
    }

    pub(crate) fn refresh_theme(&mut self) {
        self.system_dark = sys::system_dark();
        self.system_accent = sys::system_accent();
        self.theme = resolve_theme(&self.cfg, self.system_dark, self.system_accent);
        self.metrics = make_metrics(&self.cfg, &self.theme);
        self.host.set_theme(self.theme);
        self.bus_tx.send(Source::Local, EventKind::ThemeChanged);
        if self.stage.is_some() {
            self.render_frame();
        } else {
            self.update_pill();
        }
    }

    fn on_second_instance(&mut self) {
        if self.tray_hidden || self.tray.as_ref().is_none_or(|t| !t.is_added()) {
            self.show_tray();
        }
        self.on_hotkey(hotkeys::ID_PEEK);
    }

    // ----- config -----------------------------------------------------------------------------

    fn reload_config(&mut self) {
        let path = self.config_path.clone();
        std::thread::spawn(move || {
            let result = match std::fs::read_to_string(&path) {
                Ok(text) => Config::parse(&text),
                Err(e) => Err(format!("cannot read {}: {e}", path.display())),
            };
            inbox::push(Msg::Config(result));
        });
    }

    fn apply_config(&mut self, loaded: Loaded) {
        let old = std::mem::replace(&mut self.cfg, loaded.config);
        let new = self.cfg.clone();
        for w in &loaded.warnings {
            warn!("config: {w}");
        }
        crate::log::set_level(crate::log::Level::parse(&new.general.log_level));
        self.set_status(loaded.warnings.first().cloned().unwrap_or_default());

        if old.hotkeys != new.hotkeys {
            let problems = self.hotkeys.apply(self.ctrl, &new.hotkeys);
            if let Some(p) = problems.first() {
                self.set_status(p.clone());
            }
        }
        if old.general.autostart != new.general.autostart
            && let Err(e) = autostart::set(new.general.autostart)
        {
            self.set_status(format!("autostart: {e}"));
        }
        if old.general.tray_icon != new.general.tray_icon {
            if new.general.tray_icon {
                self.show_tray()
            } else if let Some(t) = self.tray.as_mut() {
                t.remove()
            }
        }
        if old.fullscreen.enabled != new.fullscreen.enabled {
            if new.fullscreen.enabled {
                self.watcher.install();
            } else {
                self.watcher.uninstall();
                self.set_fullscreen(false);
            }
        }
        if old.general.exclude_from_capture != new.general.exclude_from_capture
            && !self.opts.no_exclude
        {
            if let Some(s) = self.stage.as_mut() {
                s.set_capture_exclusion(new.general.exclude_from_capture);
            }
            if let Some(p) = self.pill.as_mut() {
                p.set_capture_exclusion(new.general.exclude_from_capture);
            }
        }
        self.theme = resolve_theme(&new, self.system_dark, self.system_accent);
        self.metrics = make_metrics(&new, &self.theme);
        self.host.set_theme(self.theme);
        self.host_ctx(clock::now());
        self.host.apply_config(Arc::new(new.clone()));
        self.services.sync(&new);
        self.bus_tx.send(Source::Local, EventKind::ConfigChanged);
        self.refresh_pages();
        self.hover.set_params(hover_params(&new));
        self.shell.gesture_mut().reverse = new.animation.reverse_scroll;
        self.shell.set_config(shell_config(&new));

        let geometry_changed = old.general.monitor != new.general.monitor
            || old.appearance.scale != new.appearance.scale
            || old.appearance.pill_width != new.appearance.pill_width
            || old.appearance.pill_height != new.appearance.pill_height
            || old.appearance.max_panel_width != new.appearance.max_panel_width
            || old.appearance.max_panel_height != new.appearance.max_panel_height;
        if geometry_changed && let Some(l) = make_layout(&new) {
            self.layout = l;
            let had = self.stage.is_some();
            self.stage = None;
            if had {
                self.warm_gpu();
            }
        }
        if self.stage.is_some() {
            self.render_frame()
        } else {
            self.update_pill()
        }
        self.start_sampling();
        info!("configuration applied");
    }

    fn drain_inbox(&mut self) {
        self.pump_bus();
        for msg in inbox::drain() {
            match msg {
                Msg::Config(Ok(l)) => self.apply_config(l),
                Msg::Config(Err(e)) => {
                    warn!("config not applied (keeping previous settings): {e}");
                    self.set_status(format!("config error: {e}"));
                }
            }
        }
    }

    // ----- the loop ---------------------------------------------------------------------------

    fn run_timers(&mut self, now: f64) {
        let mut due = Vec::new();
        self.sched.take_due(now, &mut due);
        for id in due {
            match id {
                T_HOVER => self.hover_tick(now),
                T_GPU_RELEASE => self.maybe_release_gpu(now),
                T_FS_RECHECK => self.recheck_fullscreen(),
                T_CFG_RELOAD => self.reload_config(),
                T_MODULES => {
                    self.host_ctx(now);
                    self.host.tick();
                    self.after_host();
                }
                T_REDRAW => self.redraw_once(),
                T_SCRIPT => crate::diag::step(self, now),
                _ => {}
            }
        }
        // Shell deadlines (peek expiry, staggered actions) when nothing is animating.
        if !self.animating() && self.shell.next_deadline().is_some_and(|d| d <= now) {
            self.shell.step(now);
            if self.animating() {
                self.kick("auto");
            }
        }
    }

    /// Compute what to wait on and for how long.
    fn wait_plan(&self) -> WaitPlan {
        let now = clock::now();
        let mut plan = WaitPlan {
            handles: Vec::with_capacity(2),
            frame_idx: None,
            cfg_idx: None,
            timeout: u32::MAX,
        };
        // The frame-latency handle is waited on ONLY while animating, which is what lets the loop
        // (and the process) sleep completely otherwise. After a failed frame it is skipped for a
        // moment: an unpresented swap chain keeps the handle signalled and would spin the CPU.
        let animating = self.animating();
        if animating
            && now >= self.frame_block_until
            && let Some(s) = &self.stage
        {
            plan.frame_idx = Some(plan.handles.len());
            plan.handles.push(s.frame_waitable());
        }
        if let Some(c) = &self.cfg_watch {
            plan.cfg_idx = Some(plan.handles.len());
            plan.handles.push(c.handle());
        }
        let mut deadline = self.sched.next_deadline();
        if !animating && let Some(d) = self.shell.next_deadline() {
            deadline = Some(deadline.map_or(d, |x| x.min(d)));
        }
        if let Some(d) = deadline {
            plan.timeout =
                (((d - now) * 1000.0).ceil().max(0.0) as u64).min(u32::MAX as u64 - 1) as u32;
        }
        if animating && plan.frame_idx.is_none() {
            // No usable frame handle (no GPU, or backing off after an error): tick the shell on a timer.
            plan.timeout = plan.timeout.min(16);
        }
        plan
    }

    /// Handle what woke the loop.
    fn after_wait(&mut self, signal: Signal) {
        let now = clock::now();
        match signal {
            Signal::Frame => self.render_frame(),
            Signal::Config => {
                if let Some(c) = &self.cfg_watch {
                    c.rearm();
                }
                self.sched.set(T_CFG_RELOAD, now + 0.25);
            }
            Signal::None => {}
        }
        self.run_timers(now);
        if self.animating() && (self.stage.is_none() || now < self.frame_block_until) {
            self.shell.step(now);
        }
    }
}

struct WaitPlan {
    handles: Vec<HANDLE>,
    frame_idx: Option<usize>,
    cfg_idx: Option<usize>,
    timeout: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Signal {
    None,
    Frame,
    Config,
}

// ----- entry point ------------------------------------------------------------------------------

pub fn run(opts: Options) -> i32 {
    // OLE (a single-threaded apartment) is needed for the shelf's drag and drop.
    let ole = unsafe { windows::Win32::System::Ole::OleInitialize(None) }.is_ok();
    let code = run_inner(opts);
    if ole {
        unsafe { windows::Win32::System::Ole::OleUninitialize() };
    }
    code
}

fn run_inner(opts: Options) -> i32 {
    let ctrl = match create_controller() {
        Ok(h) => h,
        Err(e) => {
            error!("cannot create the controller window: {e}");
            return 2;
        }
    };
    inbox::set_target(ctrl);
    let selftest = opts.selftest;
    let app = match App::new(ctrl, opts) {
        Ok(a) => a,
        Err(e) => {
            error!("startup failed: {e}");
            return 3;
        }
    };
    APP.with(|c| *c.borrow_mut() = Some(app));
    with_app(|a| {
        a.start();
        if selftest {
            crate::diag::begin(a);
        }
    });

    let mut exit = 0;
    'outer: while let Some(plan) = with_app(|a| a.wait_plan()) {
        let r = unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&plan.handles),
                plan.timeout,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
        let signal = if Some(idx) == plan.frame_idx {
            Signal::Frame
        } else if Some(idx) == plan.cfg_idx {
            Signal::Config
        } else {
            Signal::None
        };
        // Drain the message queue (window procedures run here and take their own short borrows).
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    exit = msg.wParam.0 as i32;
                    break 'outer;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        guard_unit(|| {
            with_app(|a| a.after_wait(signal));
        });
        // A drag-out requested by the shelf: OLE runs its own modal loop, so do it here, between
        // iterations, with no borrow of the app held (window procedures may then re-enter normally).
        if let Some(paths) = with_app(|a| a.take_pending_drag()).flatten() {
            with_app(|a| a.before_drag());
            let accepted = dragdrop::start_drag(&paths);
            with_app(|a| a.after_drag(accepted));
        }
        if let Some(code) = with_app(|a| {
            a.selftest
                .as_ref()
                .filter(|s| s.finished)
                .map(|s| s.exit_code)
        })
        .flatten()
        {
            exit = code;
            break;
        }
    }
    // Drop everything on the UI thread, in order.
    APP.with(|c| c.borrow_mut().take());
    unsafe {
        let _ = DestroyWindow(ctrl);
    }
    exit
}

fn guard_unit(f: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(f)).map_err(|_| error!("panic caught in the main loop"));
}

fn create_controller() -> windows::core::Result<HWND> {
    window::register_class(CONTROLLER_CLASS, ctrl_proc)?;
    // A hidden, never-shown top-level window: owner of the tray icon, hotkeys and notifications.
    window::create_window(
        WS_EX_TOOLWINDOW,
        CONTROLLER_CLASS,
        "Shark Notch Controller",
        WS_POPUP,
        0,
        0,
        0,
        0,
    )
}

// ----- window procedures --------------------------------------------------------------------------

unsafe extern "system" fn ctrl_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled: Option<LRESULT> = guard(|| {
        with_app(|a| {
            Some(match msg {
                WM_HOTKEY => {
                    a.on_hotkey(wp.0 as i32);
                    LRESULT(0)
                }
                WM_TRAY => {
                    a.on_tray(lp);
                    LRESULT(0)
                }
                WM_APP_WAKE => {
                    a.drain_inbox();
                    LRESULT(0)
                }
                WM_SECOND_INSTANCE => {
                    a.on_second_instance();
                    LRESULT(0)
                }
                session::WM_WTSSESSION_CHANGE => {
                    a.on_session(wp.0);
                    LRESULT(0)
                }
                session::WM_POWERBROADCAST => {
                    if wp.0 == session::PBT_POWERSETTINGCHANGE
                        && let Some(on) = unsafe { session::display_state_from(lp.0) }
                    {
                        a.on_display_state(on);
                    }
                    LRESULT(1)
                }
                WM_DISPLAYCHANGE => {
                    a.on_display_change();
                    LRESULT(0)
                }
                WM_SETTINGCHANGE => {
                    a.on_setting_change(wp.0, lp.0);
                    LRESULT(0)
                }
                WM_DWMCOLORIZATIONCOLORCHANGED => {
                    a.refresh_theme();
                    LRESULT(0)
                }
                WM_ENDSESSION => {
                    if let Some(t) = a.tray.as_mut() {
                        t.remove();
                    }
                    LRESULT(0)
                }
                m if m == a.taskbar_created && m != 0 => {
                    // Explorer restarted: the tray icon vanished with it.
                    if !a.tray_hidden {
                        a.show_tray();
                    }
                    LRESULT(0)
                }
                _ => return None,
            })
        })
        .flatten()
    });
    match handled {
        Some(r) => r,
        None if msg == WM_DESTROY => LRESULT(0),
        None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe extern "system" fn stage_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::ValidateRect(Some(hwnd), None);
            }
            return LRESULT(0);
        }
        WM_SETCURSOR => {
            if with_app(|a| a.on_set_cursor()).unwrap_or(false) {
                return LRESULT(1);
            }
        }
        WM_MOUSEMOVE | WM_MOUSELEAVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MOUSEWHEEL
        | WM_MOUSEHWHEEL => {
            guard(|| {
                with_app(|a| a.on_stage_mouse(msg, wp, lp));
            });
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe extern "system" fn pill_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// WinEvent callback target (see `win::fullscreen`).
pub fn on_win_event(event: u32, hwnd: HWND, id_object: i32, id_child: i32) {
    with_app(|a| match event {
        EVENT_SYSTEM_FOREGROUND => {
            a.watcher.track(hwnd);
            if let Some(s) = &a.stage {
                s.raise();
            }
            if let Some(p) = &a.pill {
                p.raise();
            }
            a.sched.set(T_FS_RECHECK, clock::now() + 0.05);
        }
        EVENT_OBJECT_LOCATIONCHANGE
            if id_object == 0 && id_child == 0 && hwnd == a.watcher.tracked =>
        {
            a.sched.set_earliest(T_FS_RECHECK, clock::now() + 0.12);
        }
        _ => {}
    });
}
