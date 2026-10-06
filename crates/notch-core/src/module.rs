//! The module system: a [`Module`] is *logic + layout*, never pixels; [`ModuleHost`] runs them.
//!
//! * **Lazy**: a module is only instantiated if it is enabled in the config, and it is told when it
//!   becomes visible so it can create heavy resources then — and when it has been hidden long enough
//!   that it should drop them (`on_unload`).
//! * **Event driven**: modules subscribe to event kinds; the host dispatches on the UI thread.
//!   Polling exists, but is honoured **only while the module is visible and expanded**.
//! * **Isolated**: a panic inside any module call disables that module and is reported; the notch
//!   and every other module keep running.
//! * **Testable**: modules emit [`Command`]s and draw into a [`Canvas`]; nothing here touches the OS.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use crate::civil::LocalTime;
use crate::compose::Content;
use crate::config::Config;
use crate::draw::{Canvas, HitId};
use crate::events::{Event, EventKind, EventMask, Source};
use crate::geom::{Rect, Size, Vec2};
use crate::icons::Icon;
use crate::input::Input;
use crate::theme::Theme;

pub type ModuleId = &'static str;

/// How much of a module is currently on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Not shown at all.
    Hidden,
    /// Only its chip is shown in the collapsed pill.
    Collapsed,
    /// Its page is shown in the expanded notch (or a peek banner).
    Expanded,
}

/// Playback control sent to the system's current media session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaCmd {
    PlayPause,
    Next,
    Previous,
    /// Seek to an absolute position in milliseconds.
    SeekTo(u64),
    /// Re-read the session now (position drift, a thumbnail that arrived late).
    Refresh,
}

/// Operations on the clipboard history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipCmd {
    /// Put this entry back on the system clipboard.
    Copy(u64),
    Pin(u64, bool),
    Remove(u64),
    /// Open a link entry in the default browser.
    Open(u64),
    /// Remove every unpinned entry.
    Clear,
}

/// Operations on the file shelf. The shelf only ever holds *references* to files: nothing here
/// deletes, moves or renames a user's file.
#[derive(Clone, Debug, PartialEq)]
pub enum ShelfCmd {
    /// Open a file or folder with its default application.
    Open(Arc<str>),
    /// Start a drag-and-drop of these paths out of the notch (copy/link only, never move).
    DragOut(Vec<Arc<str>>),
    /// These thumbnail images are no longer shown: free them.
    Release(Vec<u64>),
}

/// Operations on Windows notifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifCmd {
    /// Dismiss one notification (also removes it from Windows' notification centre).
    Dismiss(u64),
    ClearAll,
    /// Open Windows' "Notifications" privacy settings page.
    OpenSettings,
    /// The page was opened while access was missing: look again (the user may just have changed it).
    Recheck,
}

/// Operations on the calendar feeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalCmd {
    /// Fetch the feeds now (the page was opened and the data is stale).
    Refresh,
}

/// Operations on the system-statistics sampler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatsCmd {
    /// Take a reading now (the answer arrives as a `Stats` event). Sent only while the stats page
    /// is on screen; with `gpu` the GPU counters are read too.
    Sample { gpu: bool },
}

/// Which radio a toggle is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioKind {
    Wifi,
    Bluetooth,
}

/// Operations on the command centre's controls. Levels are 0..=1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ControlCmd {
    /// Read every control and answer with a `Control` event (sent while the page is on screen).
    Refresh,
    SetVolume(f32),
    ToggleMute,
    SetBrightness(f32),
    SetRadio {
        kind: RadioKind,
        on: bool,
    },
    /// Start Windows' screen snip (the Snipping Tool overlay).
    Snip,
    /// Open Windows' do-not-disturb settings (there is no supported way to flip it from outside).
    OpenFocusSettings,
    /// Open Windows' radio privacy settings (the toggle was refused by a privacy setting).
    OpenRadioSettings,
}

/// Operations on the small persistent store (one JSON document per key, under the app's data folder).
#[derive(Clone, Debug, PartialEq)]
pub enum StoreCmd {
    /// Read `key`; the answer arrives as a `StoreLoaded` event.
    Load(&'static str),
    /// Replace `key` (written shortly after, off the UI thread). Keys are `[a-z0-9_-]` only.
    Save { key: &'static str, data: Arc<str> },
}

/// Things a module asks the platform to do. Modules never call the OS themselves.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    OpenUrl(Arc<str>),
    /// Open the configuration file in the user's editor.
    OpenConfig,
    /// Play the system's "asterisk" sound (a timer finished).
    Chime,
    /// Show a file in Explorer (never opens or runs it).
    Reveal(Arc<str>),
    Media(MediaCmd),
    Clipboard(ClipCmd),
    Shelf(ShelfCmd),
    Notifications(NotifCmd),
    Calendar(CalCmd),
    Store(StoreCmd),
    Stats(StatsCmd),
    Control(ControlCmd),
}

/// Requests that concern the shell itself.
#[derive(Clone, Debug, PartialEq)]
pub enum ShellRequest {
    /// Show this module's peek banner for `duration` seconds.
    Peek { module: ModuleId, duration: f64 },
    /// Open the notch on this module's page.
    Expand { module: ModuleId },
}

/// Loudness of what the PC is playing, as sampled by the platform.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Audio {
    /// Not sampled this frame: no module asked for continuous frames, so no device is open.
    #[default]
    Idle,
    /// A module asked, but no audio device could be metered.
    Unavailable,
    /// Peak level 0..=1 of the default output device.
    Level(f32),
}

/// Environment values the platform supplies (the core never reads a clock or a locale itself).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Env {
    pub local: LocalTime,
    /// Current UTC time, whole seconds since 1970-01-01 (a wall clock: it also counts time spent
    /// asleep, which is what a timer the user set in minutes of real time wants).
    pub unix: i64,
    /// The user's regional preference for 24-hour time.
    pub system_24h: bool,
    /// Sampled by the platform **only while a module asks for continuous frames** (`wants_frames`).
    pub audio: Audio,
}

impl Default for Env {
    fn default() -> Self {
        Env {
            local: LocalTime::default(),
            unix: 0,
            system_24h: true,
            audio: Audio::Idle,
        }
    }
}

/// Everything a module's calls may produce, gathered by the host.
#[derive(Default, Debug)]
pub struct Out {
    pub commands: Vec<Command>,
    pub emitted: Vec<(Source, EventKind)>,
    pub shell: Vec<ShellRequest>,
    pub redraw: bool,
    /// Earliest time (monotonic seconds) a redraw is wanted even if nothing else happens.
    pub redraw_at: Option<f64>,
    /// A module asked for (`true`) or gave up (`false`) keyboard focus.
    pub keyboard: Option<(ModuleId, bool)>,
}

/// Context handed to non-drawing calls.
pub struct Cx<'a> {
    pub now: f64,
    pub env: &'a Env,
    pub theme: &'a Theme,
    pub config: &'a Config,
    module: ModuleId,
    out: &'a mut Out,
}

impl<'a> Cx<'a> {
    /// Build a context outside the host (for unit-testing a module in isolation).
    pub fn for_test(
        now: f64,
        env: &'a Env,
        theme: &'a Theme,
        config: &'a Config,
        out: &'a mut Out,
    ) -> Cx<'a> {
        Cx {
            now,
            env,
            theme,
            config,
            module: "test",
            out,
        }
    }

    pub fn command(&mut self, c: Command) {
        self.out.commands.push(c);
    }

    /// Publish an event to every subscriber (including from a module to other modules).
    pub fn emit(&mut self, source: Source, kind: EventKind) {
        self.out.emitted.push((source, kind));
    }

    /// Something visible changed: draw one more frame (no animation loop is started).
    pub fn request_redraw(&mut self) {
        self.out.redraw = true;
    }

    /// Draw a frame at (or after) monotonic time `t`; the earliest request wins.
    pub fn redraw_at(&mut self, t: f64) {
        self.out.redraw_at = Some(self.out.redraw_at.map_or(t, |x| x.min(t)));
    }

    /// Ask the shell to show this module's peek banner briefly.
    pub fn peek(&mut self, duration: f64) {
        self.out.shell.push(ShellRequest::Peek {
            module: self.module,
            duration,
        });
    }

    /// Ask for keyboard input (`Input::Char` / `Input::Key`) while a text field is being edited. The
    /// platform makes the window focusable only for as long as this is on; give it up (`false`) as
    /// soon as editing ends. Input is delivered to the module that asked.
    pub fn request_keyboard(&mut self, on: bool) {
        self.out.keyboard = Some((self.module, on));
    }

    /// Ask the shell to open on this module's page.
    pub fn request_expand(&mut self) {
        self.out.shell.push(ShellRequest::Expand {
            module: self.module,
        });
    }
}

/// Context handed to drawing calls.
pub struct DrawCx<'a> {
    pub now: f64,
    pub env: &'a Env,
    pub config: &'a Config,
}

pub trait Module {
    fn id(&self) -> ModuleId;
    fn title(&self) -> &'static str;
    fn icon(&self) -> Icon;

    /// Event kinds this module wants to receive.
    fn subscriptions(&self) -> EventMask {
        EventMask::NONE
    }
    /// Honoured **only while the module is visible and expanded**.
    fn poll_interval(&self) -> Option<Duration> {
        None
    }
    /// `true` while the module needs continuous frames (e.g. a visualizer). Only consulted while the
    /// module is visible and expanded.
    fn wants_frames(&self) -> bool {
        false
    }
    /// Whether the module's page is currently in the page ring (e.g. media only with a session).
    fn page_visible(&self) -> bool {
        true
    }

    /// Size of the expanded page, in DIPs (including nothing for chrome: the shell adds padding).
    fn expanded_size(&self) -> Size;
    fn peek_size(&self) -> Option<Size> {
        None
    }
    /// Width of the module's chip in the collapsed pill, if it has something to show.
    fn chip_width(&self) -> Option<f32> {
        None
    }
    /// Higher priority chips win the (two) chip slots.
    fn chip_priority(&self) -> i32 {
        0
    }

    /// Called once, right after the host created the module: restore saved state, ask for data.
    fn on_start(&mut self, _cx: &mut Cx) {}
    /// The monotonic time (seconds) at which `on_tick` should run, **whether or not the module is
    /// visible** (a meeting about to start, a timer about to finish). `None` = no wake-up needed. Keep
    /// it cheap; it is asked after every host call.
    fn next_wake(&self, _now: f64, _env: &Env) -> Option<f64> {
        None
    }
    fn on_tick(&mut self, _cx: &mut Cx) {}
    fn on_event(&mut self, _ev: &Event, _cx: &mut Cx) {}
    fn on_poll(&mut self, _cx: &mut Cx) {}
    fn on_visibility(&mut self, _v: Visibility, _cx: &mut Cx) {}
    /// Called once after the module has been hidden for a while: free heavy resources.
    fn on_unload(&mut self, _cx: &mut Cx) {}
    fn on_config(&mut self, _cfg: &Config, _cx: &mut Cx) {}
    fn on_suspend(&mut self, _cx: &mut Cx) {}
    fn on_resume(&mut self, _cx: &mut Cx) {}
    /// Return `true` if the input was consumed.
    fn on_input(&mut self, _hit: Option<HitId>, _input: &Input, _cx: &mut Cx) -> bool {
        false
    }
    /// Pointer input over this module's peek banner (it is click-through until the pointer enters
    /// it). Return `true` if consumed; an unconsumed click opens the module's page.
    fn on_peek_input(&mut self, _hit: Option<HitId>, _input: &Input, _cx: &mut Cx) -> bool {
        false
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx);
    fn draw_chip(&mut self, _cv: &mut Canvas, _area: Rect, _dx: &DrawCx) {}
    fn draw_peek(&mut self, _cv: &mut Canvas, _area: Rect, _dx: &DrawCx) {}
}

/// Creates a module if the config enables it (`None` = disabled; nothing is instantiated).
#[derive(Clone, Copy)]
pub struct Factory {
    pub id: ModuleId,
    pub create: fn(&Config) -> Option<Box<dyn Module>>,
}

struct Entry {
    id: ModuleId,
    module: Box<dyn Module>,
    vis: Visibility,
    hidden_since: Option<f64>,
    loaded: bool,
    poisoned: bool,
    next_poll: Option<f64>,
    started: bool,
    /// When `on_tick` last ran (guards against a module that keeps asking for a past wake-up).
    last_tick: f64,
}

/// How long a module may stay hidden before `on_unload` is called.
pub const UNLOAD_AFTER: f64 = 30.0;
/// Least time between two `on_tick` calls of one module.
const MIN_TICK_GAP: f64 = 0.25;
/// Chip layout metrics (DIPs).
pub const CHIP_PAD: f32 = 12.0;
pub const CHIP_GAP: f32 = 10.0;
const MAX_CHIPS: usize = 2;

pub struct ModuleHost {
    factories: Vec<Factory>,
    entries: Vec<Entry>,
    config: Arc<Config>,
    theme: Theme,
    env: Env,
    now: f64,
    out: Out,
    poisoned: Vec<ModuleId>,
}

/// Borrowed pieces of the host needed to build a [`Cx`] while an entry is mutably borrowed.
struct Ctx<'a> {
    now: f64,
    env: &'a Env,
    theme: &'a Theme,
    config: &'a Config,
}

fn call<R>(
    e: &mut Entry,
    ctx: &Ctx,
    out: &mut Out,
    poisoned: &mut Vec<ModuleId>,
    f: impl FnOnce(&mut dyn Module, &mut Cx) -> R,
) -> Option<R> {
    if e.poisoned {
        return None;
    }
    let id = e.id;
    let mut cx = Cx {
        now: ctx.now,
        env: ctx.env,
        theme: ctx.theme,
        config: ctx.config,
        module: id,
        out,
    };
    match catch_unwind(AssertUnwindSafe(|| f(e.module.as_mut(), &mut cx))) {
        Ok(r) => Some(r),
        Err(_) => {
            e.poisoned = true;
            poisoned.push(id);
            None
        }
    }
}

impl ModuleHost {
    pub fn new(factories: Vec<Factory>, config: Arc<Config>, theme: Theme) -> ModuleHost {
        let mut h = ModuleHost {
            factories,
            entries: Vec::new(),
            config: config.clone(),
            theme,
            env: Env::default(),
            now: 0.0,
            out: Out::default(),
            poisoned: Vec::new(),
        };
        h.apply_config(config);
        h
    }

    /// Feed the host the current time and environment before calling into it.
    pub fn set_context(&mut self, now: f64, env: Env) {
        self.now = now;
        self.env = env;
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// (Re)build the module list from `cfg`: instantiate newly enabled modules (and only those),
    /// drop disabled ones, keep the state of the rest, and order pages by `modules.order`.
    pub fn apply_config(&mut self, cfg: Arc<Config>) {
        self.config = cfg.clone();
        let mut old: Vec<Entry> = std::mem::take(&mut self.entries);
        for id in &cfg.modules.order {
            let Some(f) = self.factories.iter().find(|f| f.id == id.as_str()).copied() else {
                continue;
            };
            if self.entries.iter().any(|e| e.id == f.id) {
                continue; // listed twice
            }
            if let Some(pos) = old.iter().position(|e| e.id == f.id) {
                // Still enabled? Ask the factory again (it checks the config).
                if (f.create)(&cfg).is_some() {
                    let mut e = old.remove(pos);
                    let ctx = Ctx {
                        now: self.now,
                        env: &self.env,
                        theme: &self.theme,
                        config: &cfg,
                    };
                    call(&mut e, &ctx, &mut self.out, &mut self.poisoned, |m, cx| {
                        m.on_config(&cfg, cx)
                    });
                    self.entries.push(e);
                }
            } else if let Some(module) = (f.create)(&cfg) {
                self.entries.push(Entry {
                    id: f.id,
                    module,
                    vis: Visibility::Hidden,
                    hidden_since: Some(self.now),
                    loaded: false,
                    poisoned: false,
                    next_poll: None,
                    started: false,
                    last_tick: f64::NEG_INFINITY,
                });
            }
        }
        // Whatever remains in `old` was disabled or removed from the order: dropped here.
    }

    /// Run `on_start` for modules created since the last call. The platform calls this after it has
    /// given the host a context (start-up and after every configuration reload).
    pub fn start_new(&mut self) {
        for i in 0..self.entries.len() {
            if self.entries[i].started {
                continue;
            }
            self.entries[i].started = true;
            let ctx = Ctx {
                now: self.now,
                env: &self.env,
                theme: &self.theme,
                config: &self.config,
            };
            call(
                &mut self.entries[i],
                &ctx,
                &mut self.out,
                &mut self.poisoned,
                |m, cx| m.on_start(cx),
            );
        }
    }

    // ----- pages ------------------------------------------------------------------------------

    fn page_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.poisoned && e.module.page_visible())
            .map(|(i, _)| i)
            .collect()
    }

    /// Expanded sizes of the pages in the ring.
    pub fn pages(&self) -> Vec<Size> {
        self.page_indices()
            .into_iter()
            .map(|i| self.entries[i].module.expanded_size())
            .collect()
    }

    pub fn page_ids(&self) -> Vec<ModuleId> {
        self.page_indices()
            .into_iter()
            .map(|i| self.entries[i].id)
            .collect()
    }

    /// Page index (within the ring) of a module, if it is in the ring.
    pub fn page_of(&self, id: ModuleId) -> Option<usize> {
        self.page_ids().iter().position(|p| *p == id)
    }

    pub fn module_ids(&self) -> Vec<ModuleId> {
        self.entries.iter().map(|e| e.id).collect()
    }

    pub fn peek_size(&self, id: ModuleId) -> Option<Size> {
        self.entries
            .iter()
            .find(|e| e.id == id && !e.poisoned)
            .and_then(|e| e.module.peek_size())
    }

    /// Modules disabled by a panic since the last call.
    pub fn take_poisoned(&mut self) -> Vec<ModuleId> {
        std::mem::take(&mut self.poisoned)
    }

    // ----- events -----------------------------------------------------------------------------

    /// Deliver events to subscribers. Events emitted by modules are delivered too (up to 4 rounds, so
    /// a feedback loop between two modules cannot hang the UI).
    pub fn dispatch(&mut self, events: Vec<Event>) {
        let mut queue = events;
        for _ in 0..4 {
            if queue.is_empty() {
                break;
            }
            for ev in &queue {
                let kind = ev.kind.kind();
                for i in 0..self.entries.len() {
                    if !self.entries[i].module.subscriptions().contains(kind) {
                        continue;
                    }
                    let ctx = Ctx {
                        now: self.now,
                        env: &self.env,
                        theme: &self.theme,
                        config: &self.config,
                    };
                    call(
                        &mut self.entries[i],
                        &ctx,
                        &mut self.out,
                        &mut self.poisoned,
                        |m, cx| m.on_event(ev, cx),
                    );
                }
            }
            queue = std::mem::take(&mut self.out.emitted)
                .into_iter()
                .map(|(s, k)| Event::new(s, k))
                .collect();
        }
    }

    // ----- visibility, polling, unloading ---------------------------------------------------------

    /// Tell the host what is on screen. `expanded_page` is the ring index of the page shown while the
    /// notch is expanded; `chips` lists whether the collapsed pill is showing chips.
    pub fn set_view(&mut self, expanded_page: Option<usize>) {
        let pages = self.page_indices();
        let active = expanded_page.and_then(|p| pages.get(p).copied());
        let chip_ids = self
            .chip_slots()
            .into_iter()
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        for i in 0..self.entries.len() {
            let want = if Some(i) == active {
                Visibility::Expanded
            } else if expanded_page.is_none() && chip_ids.contains(&i) {
                Visibility::Collapsed
            } else {
                Visibility::Hidden
            };
            self.set_visibility(i, want);
        }
    }

    fn set_visibility(&mut self, i: usize, want: Visibility) {
        let now = self.now;
        let ctx = Ctx {
            now,
            env: &self.env,
            theme: &self.theme,
            config: &self.config,
        };
        let e = &mut self.entries[i];
        if e.vis == want || e.poisoned {
            return;
        }
        e.vis = want;
        e.loaded = e.loaded || want != Visibility::Hidden;
        e.hidden_since = (want == Visibility::Hidden).then_some(now);
        e.next_poll = (want == Visibility::Expanded).then_some(now);
        call(e, &ctx, &mut self.out, &mut self.poisoned, |m, cx| {
            m.on_visibility(want, cx)
        });
    }

    /// Earliest time something needs the host: a visible+expanded module's poll, a module's own
    /// wake-up (`next_wake`), or an unload.
    pub fn next_deadline(&self) -> Option<f64> {
        let mut best: Option<f64> = None;
        let mut take = |t: f64| best = Some(best.map_or(t, |b: f64| b.min(t)));
        for e in self.entries.iter().filter(|e| !e.poisoned) {
            if let Some(t) = e.module.next_wake(self.now, &self.env) {
                // A module that keeps naming a moment that has passed must not make the loop spin.
                take(t.max(e.last_tick + MIN_TICK_GAP));
            }
            if e.vis == Visibility::Expanded
                && let Some(t) = e.next_poll
            {
                take(t);
            }
            if e.vis == Visibility::Hidden
                && e.loaded
                && let Some(since) = e.hidden_since
            {
                take(since + UNLOAD_AFTER);
            }
        }
        best
    }

    /// Run polls that are due (only for visible+expanded modules) and unload long-hidden modules.
    pub fn tick(&mut self) {
        let now = self.now;
        for i in 0..self.entries.len() {
            let ctx = Ctx {
                now,
                env: &self.env,
                theme: &self.theme,
                config: &self.config,
            };
            let e = &mut self.entries[i];
            if e.poisoned {
                continue;
            }
            if let Some(t) = e.module.next_wake(now, &self.env)
                && now >= t.max(e.last_tick + MIN_TICK_GAP)
            {
                e.last_tick = now;
                call(e, &ctx, &mut self.out, &mut self.poisoned, |m, cx| {
                    m.on_tick(cx)
                });
                if e.poisoned {
                    continue;
                }
            }
            if e.vis == Visibility::Expanded
                && let Some(t) = e.next_poll
                && now >= t
            {
                match e.module.poll_interval() {
                    Some(d) => {
                        e.next_poll = Some(now + d.as_secs_f64());
                        call(e, &ctx, &mut self.out, &mut self.poisoned, |m, cx| {
                            m.on_poll(cx)
                        });
                    }
                    None => e.next_poll = None,
                }
            }
            if e.vis == Visibility::Hidden
                && e.loaded
                && e.hidden_since.is_some_and(|s| now - s >= UNLOAD_AFTER)
            {
                e.loaded = false;
                call(e, &ctx, &mut self.out, &mut self.poisoned, |m, cx| {
                    m.on_unload(cx)
                });
            }
        }
    }

    /// Whether any visible, expanded module needs continuous frames.
    pub fn wants_frames(&self) -> bool {
        self.entries
            .iter()
            .any(|e| !e.poisoned && e.vis == Visibility::Expanded && e.module.wants_frames())
    }

    pub fn suspend(&mut self) {
        self.each(|m, cx| m.on_suspend(cx));
    }

    pub fn resume(&mut self) {
        self.each(|m, cx| m.on_resume(cx));
    }

    fn each(&mut self, f: impl Fn(&mut dyn Module, &mut Cx)) {
        for i in 0..self.entries.len() {
            let ctx = Ctx {
                now: self.now,
                env: &self.env,
                theme: &self.theme,
                config: &self.config,
            };
            call(
                &mut self.entries[i],
                &ctx,
                &mut self.out,
                &mut self.poisoned,
                &f,
            );
        }
    }

    // ----- input ------------------------------------------------------------------------------

    /// Route input to the module on ring page `page`. Returns whether it was consumed.
    pub fn input(&mut self, page: usize, hit: Option<HitId>, input: &Input) -> bool {
        let Some(&i) = self.page_indices().get(page) else {
            return false;
        };
        let ctx = Ctx {
            now: self.now,
            env: &self.env,
            theme: &self.theme,
            config: &self.config,
        };
        call(
            &mut self.entries[i],
            &ctx,
            &mut self.out,
            &mut self.poisoned,
            |m, cx| m.on_input(hit, input, cx),
        )
        .unwrap_or(false)
    }

    /// Route pointer input over a peek banner to the module that owns it (`owner` is the id the
    /// shell reports in its frame). Returns whether it was consumed.
    pub fn peek_input(&mut self, owner: u32, hit: Option<HitId>, input: &Input) -> bool {
        let i = owner as usize;
        if i >= self.entries.len() {
            return false;
        }
        let ctx = Ctx {
            now: self.now,
            env: &self.env,
            theme: &self.theme,
            config: &self.config,
        };
        call(
            &mut self.entries[i],
            &ctx,
            &mut self.out,
            &mut self.poisoned,
            |m, cx| m.on_peek_input(hit, input, cx),
        )
        .unwrap_or(false)
    }

    /// Deliver typed input to the module that asked for the keyboard.
    pub fn keyboard_input(&mut self, id: ModuleId, input: &Input) -> bool {
        let Some(i) = self.entries.iter().position(|e| e.id == id) else {
            return false;
        };
        let ctx = Ctx {
            now: self.now,
            env: &self.env,
            theme: &self.theme,
            config: &self.config,
        };
        call(
            &mut self.entries[i],
            &ctx,
            &mut self.out,
            &mut self.poisoned,
            |m, cx| m.on_input(None, input, cx),
        )
        .unwrap_or(false)
    }

    // ----- chips ------------------------------------------------------------------------------

    /// The (up to two) chips shown in the collapsed pill: `(entry index, width)`, highest priority first.
    fn chip_slots(&self) -> Vec<(usize, f32)> {
        let mut v: Vec<(usize, f32, i32)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.poisoned)
            .filter_map(|(i, e)| {
                e.module
                    .chip_width()
                    .map(|w| (i, w, e.module.chip_priority()))
            })
            .collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.2));
        v.truncate(MAX_CHIPS);
        // Draw left-to-right in page order for a stable layout.
        v.sort_by_key(|c| c.0);
        v.into_iter().map(|(i, w, _)| (i, w)).collect()
    }

    /// Which modules have a chip in the pill right now (for diagnostics).
    pub fn chip_owners(&self) -> Vec<ModuleId> {
        self.chip_slots()
            .into_iter()
            .map(|(i, _)| self.entries[i].module.id())
            .collect()
    }

    /// Total pill width needed for the current chips (0 = none).
    pub fn chips_width(&self) -> f32 {
        let slots = self.chip_slots();
        if slots.is_empty() {
            return 0.0;
        }
        let widths: f32 = slots.iter().map(|s| s.1).sum();
        widths + CHIP_GAP * (slots.len() as f32 - 1.0) + CHIP_PAD * 2.0
    }

    // ----- results ----------------------------------------------------------------------------

    pub fn take_out(&mut self) -> Out {
        std::mem::take(&mut self.out)
    }

    fn draw_ctx(&self) -> (f64, Env, Arc<Config>) {
        (self.now, self.env, self.config.clone())
    }
}

impl Content for ModuleHost {
    fn draw_page(&mut self, page: usize, cv: &mut Canvas, area: Rect) {
        let Some(&i) = self.page_indices().get(page) else {
            return;
        };
        let (now, env, cfg) = self.draw_ctx();
        let dx = DrawCx {
            now,
            env: &env,
            config: &cfg,
        };
        self.guarded_draw(i, |m| m.draw_expanded(cv, area, &dx));
    }

    fn draw_peek(&mut self, owner: u32, cv: &mut Canvas, area: Rect) {
        let i = owner as usize;
        if i >= self.entries.len() {
            return;
        }
        let (now, env, cfg) = self.draw_ctx();
        let dx = DrawCx {
            now,
            env: &env,
            config: &cfg,
        };
        self.guarded_draw(i, |m| m.draw_peek(cv, area, &dx));
    }

    fn draw_chips(&mut self, cv: &mut Canvas, area: Rect) {
        let slots = self.chip_slots();
        if slots.is_empty() {
            return;
        }
        let total: f32 =
            slots.iter().map(|s| s.1).sum::<f32>() + CHIP_GAP * (slots.len() as f32 - 1.0);
        let mut x = area.x + (area.w - total) * 0.5;
        let (now, env, cfg) = self.draw_ctx();
        let dx = DrawCx {
            now,
            env: &env,
            config: &cfg,
        };
        for (i, w) in slots {
            let rect = Rect::new(x, area.y, w, area.h);
            self.guarded_draw(i, |m| m.draw_chip(cv, rect, &dx));
            x += w + CHIP_GAP;
        }
    }

    fn page_count(&self) -> usize {
        self.page_indices().len()
    }
}

impl ModuleHost {
    fn guarded_draw(&mut self, i: usize, f: impl FnOnce(&mut dyn Module)) {
        let e = &mut self.entries[i];
        if e.poisoned {
            return;
        }
        if catch_unwind(AssertUnwindSafe(|| f(e.module.as_mut()))).is_err() {
            e.poisoned = true;
            self.poisoned.push(e.id);
        }
    }

    /// The owner id used for a module's peek banner (`Content::draw_peek` receives it back).
    pub fn peek_owner(&self, id: ModuleId) -> Option<u32> {
        self.entries
            .iter()
            .position(|e| e.id == id)
            .map(|i| i as u32)
    }

    /// The module a peek `owner` id (as reported by the shell's frame) belongs to.
    pub fn owner_id(&self, owner: u32) -> Option<ModuleId> {
        self.entries.get(owner as usize).map(|e| e.id)
    }

    /// Centre of an expanded page area, handy for tests of hit routing.
    pub fn page_center(area: Rect) -> Vec2 {
        area.center()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawList, TextStyle};
    use crate::events::{BatteryInfo, FocusInfo};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A scriptable fake module that records what the host does to it.
    #[derive(Default)]
    struct Log {
        events: Vec<(Source, String)>,
        visibility: Vec<Visibility>,
        polls: u32,
        unloads: u32,
        configs: u32,
        inputs: Vec<Option<HitId>>,
        suspended: u32,
        starts: u32,
        ticks: u32,
        peek_inputs: u32,
    }

    struct Fake {
        id: ModuleId,
        log: Rc<RefCell<Log>>,
        size: Size,
        subs: EventMask,
        poll: Option<Duration>,
        chip: Option<f32>,
        prio: i32,
        visible: bool,
        frames: bool,
        panic_on_event: bool,
        panic_on_draw: bool,
        emit_on_event: bool,
        wake: Option<f64>,
        keyboard_on_start: bool,
    }

    impl Fake {
        fn new(id: ModuleId, log: &Rc<RefCell<Log>>) -> Fake {
            Fake {
                id,
                log: log.clone(),
                size: Size::new(300.0, 100.0),
                subs: EventMask::NONE,
                poll: None,
                chip: None,
                prio: 0,
                visible: true,
                frames: false,
                panic_on_event: false,
                panic_on_draw: false,
                emit_on_event: false,
                wake: None,
                keyboard_on_start: false,
            }
        }
    }

    impl Module for Fake {
        fn id(&self) -> ModuleId {
            self.id
        }
        fn title(&self) -> &'static str {
            "fake"
        }
        fn icon(&self) -> Icon {
            Icon::Clock
        }
        fn subscriptions(&self) -> EventMask {
            self.subs
        }
        fn poll_interval(&self) -> Option<Duration> {
            self.poll
        }
        fn wants_frames(&self) -> bool {
            self.frames
        }
        fn page_visible(&self) -> bool {
            self.visible
        }
        fn expanded_size(&self) -> Size {
            self.size
        }
        fn chip_width(&self) -> Option<f32> {
            self.chip
        }
        fn chip_priority(&self) -> i32 {
            self.prio
        }
        fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
            if self.panic_on_event {
                panic!("boom");
            }
            self.log
                .borrow_mut()
                .events
                .push((ev.source, format!("{:?}", ev.kind.kind())));
            if self.emit_on_event {
                cx.emit(Source::Local, EventKind::ThemeChanged);
            }
        }
        fn on_start(&mut self, cx: &mut Cx) {
            self.log.borrow_mut().starts += 1;
            if self.keyboard_on_start {
                cx.request_keyboard(true);
            }
        }
        fn next_wake(&self, _now: f64, _env: &Env) -> Option<f64> {
            self.wake
        }
        fn on_tick(&mut self, _cx: &mut Cx) {
            self.log.borrow_mut().ticks += 1;
            // Deliberately leaves `wake` in the past: the host must not spin on it.
        }
        fn on_peek_input(&mut self, hit: Option<HitId>, _i: &Input, _cx: &mut Cx) -> bool {
            self.log.borrow_mut().peek_inputs += 1;
            hit.is_some()
        }
        fn on_poll(&mut self, cx: &mut Cx) {
            self.log.borrow_mut().polls += 1;
            cx.request_redraw();
        }
        fn on_visibility(&mut self, v: Visibility, _cx: &mut Cx) {
            self.log.borrow_mut().visibility.push(v);
        }
        fn on_unload(&mut self, _cx: &mut Cx) {
            self.log.borrow_mut().unloads += 1;
        }
        fn on_config(&mut self, _cfg: &Config, _cx: &mut Cx) {
            self.log.borrow_mut().configs += 1;
        }
        fn on_suspend(&mut self, _cx: &mut Cx) {
            self.log.borrow_mut().suspended += 1;
        }
        fn on_input(&mut self, hit: Option<HitId>, _i: &Input, _cx: &mut Cx) -> bool {
            self.log.borrow_mut().inputs.push(hit);
            hit.is_some()
        }
        fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, _dx: &DrawCx) {
            if self.panic_on_draw {
                panic!("draw boom");
            }
            let th = *cv.theme;
            cv.text(area, self.id, TextStyle::body(), th.text);
            cv.hit(area, HitId(7), crate::draw::CursorKind::Hand);
        }
        fn draw_chip(&mut self, cv: &mut Canvas, area: Rect, _dx: &DrawCx) {
            let th = *cv.theme;
            cv.text(area, self.id, TextStyle::caption(), th.text);
        }
    }

    fn cfg_with(order: &[&str]) -> Arc<Config> {
        let mut c = Config::default();
        c.modules.order = order.iter().map(|s| s.to_string()).collect();
        Arc::new(c)
    }

    /// Build a host directly from prepared entries (bypasses factories so tests can configure fakes).
    fn host_with(fakes: Vec<Fake>) -> ModuleHost {
        let mut h = ModuleHost::new(Vec::new(), cfg_with(&[]), Theme::default());
        for f in fakes {
            let id = f.id;
            h.entries.push(Entry {
                id,
                module: Box::new(f),
                vis: Visibility::Hidden,
                hidden_since: Some(0.0),
                loaded: false,
                poisoned: false,
                next_poll: None,
                started: false,
                last_tick: f64::NEG_INFINITY,
            });
        }
        h
    }

    fn ev(source: Source, kind: EventKind) -> Event {
        Event::new(source, kind)
    }

    #[test]
    fn on_start_runs_once_per_module() {
        let log = Rc::new(RefCell::new(Log::default()));
        let mut h = host_with(vec![Fake::new("a", &log)]);
        h.start_new();
        h.start_new();
        assert_eq!(log.borrow().starts, 1);
        // A module added by a later configuration reload is started too, the old one is not again.
        h.entries.push(Entry {
            id: "b",
            module: Box::new(Fake::new("b", &log)),
            vis: Visibility::Hidden,
            hidden_since: Some(0.0),
            loaded: false,
            poisoned: false,
            next_poll: None,
            started: false,
            last_tick: f64::NEG_INFINITY,
        });
        h.start_new();
        assert_eq!(log.borrow().starts, 2);
    }

    #[test]
    fn a_modules_own_wake_up_drives_the_deadline_and_ticks_even_when_hidden() {
        let log = Rc::new(RefCell::new(Log::default()));
        let mut f = Fake::new("a", &log);
        f.wake = Some(50.0);
        let mut h = host_with(vec![f]);
        h.set_context(10.0, Env::default());
        assert_eq!(h.next_deadline(), Some(50.0));
        h.tick();
        assert_eq!(log.borrow().ticks, 0, "not due yet");
        h.set_context(50.0, Env::default());
        h.tick();
        assert_eq!(log.borrow().ticks, 1, "due while the module is hidden");
    }

    #[test]
    fn a_module_naming_a_past_wake_up_cannot_make_the_loop_spin() {
        let log = Rc::new(RefCell::new(Log::default()));
        let mut f = Fake::new("a", &log);
        f.wake = Some(5.0); // forever in the past: the fake never advances it
        let mut h = host_with(vec![f]);
        h.set_context(100.0, Env::default());
        h.tick();
        assert_eq!(log.borrow().ticks, 1);
        // The next deadline is pushed out instead of being "now" again.
        let next = h.next_deadline().unwrap();
        assert!(next >= 100.0 + MIN_TICK_GAP - 1e-9, "{next}");
        h.set_context(100.1, Env::default());
        h.tick();
        assert_eq!(log.borrow().ticks, 1, "throttled");
        h.set_context(100.3, Env::default());
        h.tick();
        assert_eq!(log.borrow().ticks, 2);
    }

    #[test]
    fn peek_and_keyboard_input_reach_the_right_module() {
        let log = Rc::new(RefCell::new(Log::default()));
        let mut h = host_with(vec![Fake::new("a", &log), Fake::new("b", &log)]);
        let owner = h.peek_owner("b").unwrap();
        assert!(h.peek_input(owner, Some(HitId(1)), &Input::Click(Vec2::ZERO)));
        assert!(!h.peek_input(owner, None, &Input::Click(Vec2::ZERO)));
        assert_eq!(log.borrow().peek_inputs, 2);
        assert!(!h.peek_input(99, None, &Input::Leave), "unknown owner");
        assert!(!h.keyboard_input("nope", &Input::Char('x')));
        h.keyboard_input("a", &Input::Char('x'));
        assert_eq!(
            log.borrow().inputs,
            vec![None],
            "delivered without a hit region"
        );
    }

    #[test]
    fn a_module_can_ask_for_the_keyboard() {
        let log = Rc::new(RefCell::new(Log::default()));
        let mut f = Fake::new("a", &log);
        f.keyboard_on_start = true;
        let mut h = host_with(vec![f]);
        h.start_new();
        assert_eq!(h.take_out().keyboard, Some(("a", true)));
    }

    #[test]
    fn only_enabled_modules_are_instantiated() {
        thread_local! { static CREATED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) }; }
        fn make_a(cfg: &Config) -> Option<Box<dyn Module>> {
            CREATED.with(|c| c.borrow_mut().push("a"));
            cfg.modules.order.contains(&"a".to_string()).then(|| {
                Box::new(Fake::new("a", &Rc::new(RefCell::new(Log::default())))) as Box<dyn Module>
            })
        }
        fn make_b(_: &Config) -> Option<Box<dyn Module>> {
            CREATED.with(|c| c.borrow_mut().push("b"));
            None // disabled by config
        }
        let h = ModuleHost::new(
            vec![
                Factory {
                    id: "a",
                    create: make_a,
                },
                Factory {
                    id: "b",
                    create: make_b,
                },
            ],
            cfg_with(&["a", "b", "ghost"]),
            Theme::default(),
        );
        assert_eq!(
            h.module_ids(),
            vec!["a"],
            "b is disabled, ghost has no factory"
        );
        assert_eq!(h.page_count(), 1);
    }

    #[test]
    fn dispatch_routes_by_subscription_and_keeps_the_source() {
        let (la, lb) = (
            Rc::new(RefCell::new(Log::default())),
            Rc::new(RefCell::new(Log::default())),
        );
        let mut a = Fake::new("a", &la);
        a.subs = EventMask::of(&[crate::events::Kind::Battery]);
        let mut b = Fake::new("b", &lb);
        b.subs = EventMask::of(&[
            crate::events::Kind::FocusChanged,
            crate::events::Kind::Battery,
        ]);
        let mut h = host_with(vec![a, b]);
        h.dispatch(vec![
            ev(
                Source::Phone,
                EventKind::Battery(BatteryInfo {
                    percent: 42,
                    charging: true,
                }),
            ),
            ev(
                Source::Local,
                EventKind::FocusChanged(FocusInfo {
                    name: "Work".into(),
                    active: true,
                }),
            ),
            ev(Source::Local, EventKind::ThemeChanged),
        ]);
        assert_eq!(
            la.borrow().events,
            vec![(Source::Phone, "Battery".to_string())]
        );
        assert_eq!(
            lb.borrow().events,
            vec![
                (Source::Phone, "Battery".to_string()),
                (Source::Local, "FocusChanged".to_string())
            ]
        );
    }

    #[test]
    fn module_emitted_events_reach_other_modules_but_cannot_loop_forever() {
        let (la, lb) = (
            Rc::new(RefCell::new(Log::default())),
            Rc::new(RefCell::new(Log::default())),
        );
        let mut a = Fake::new("a", &la);
        a.subs = EventMask::of(&[crate::events::Kind::Battery]);
        a.emit_on_event = true;
        let mut b = Fake::new("b", &lb);
        b.subs = EventMask::of(&[crate::events::Kind::ThemeChanged]);
        let mut h = host_with(vec![a, b]);
        h.dispatch(vec![ev(
            Source::Local,
            EventKind::Battery(BatteryInfo {
                percent: 1,
                charging: false,
            }),
        )]);
        assert_eq!(
            lb.borrow().events.len(),
            1,
            "a's emitted ThemeChanged was delivered to b"
        );

        // A pair of modules that keep re-emitting what the other listens to must terminate.
        let (lc, ld) = (
            Rc::new(RefCell::new(Log::default())),
            Rc::new(RefCell::new(Log::default())),
        );
        let mut c = Fake::new("c", &lc);
        c.subs = EventMask::of(&[crate::events::Kind::ThemeChanged]);
        c.emit_on_event = true; // emits ThemeChanged on ThemeChanged: a self-feeding loop
        let mut d = Fake::new("d", &ld);
        d.subs = EventMask::of(&[crate::events::Kind::ThemeChanged]);
        let mut h2 = host_with(vec![c, d]);
        h2.dispatch(vec![ev(Source::Local, EventKind::ThemeChanged)]);
        assert!(
            lc.borrow().events.len() <= 4,
            "bounded rounds: {}",
            lc.borrow().events.len()
        );
    }

    #[test]
    fn a_panicking_module_is_disabled_and_others_keep_running() {
        let (la, lb) = (
            Rc::new(RefCell::new(Log::default())),
            Rc::new(RefCell::new(Log::default())),
        );
        let mut bad = Fake::new("bad", &la);
        bad.subs = EventMask::of(&[crate::events::Kind::Battery]);
        bad.panic_on_event = true;
        let mut good = Fake::new("good", &lb);
        good.subs = EventMask::of(&[crate::events::Kind::Battery]);
        let mut h = host_with(vec![bad, good]);
        let e = || {
            ev(
                Source::Local,
                EventKind::Battery(BatteryInfo {
                    percent: 5,
                    charging: false,
                }),
            )
        };
        h.dispatch(vec![e()]);
        assert_eq!(h.take_poisoned(), vec!["bad"]);
        assert_eq!(
            lb.borrow().events.len(),
            1,
            "the healthy module still got the event"
        );
        h.dispatch(vec![e()]);
        assert_eq!(lb.borrow().events.len(), 2);
        assert!(h.take_poisoned().is_empty(), "poisoned once");
        assert_eq!(
            h.page_ids(),
            vec!["good"],
            "the broken module leaves the page ring"
        );
    }

    #[test]
    fn a_panicking_draw_is_contained_too() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut bad = Fake::new("bad", &l);
        bad.panic_on_draw = true;
        let mut h = host_with(vec![bad, Fake::new("ok", &l)]);
        let theme = Theme::default();
        let mut list = DrawList::new();
        let mut cv = Canvas::new(&mut list, &theme);
        h.draw_page(0, &mut cv, Rect::new(0.0, 0.0, 100.0, 50.0));
        assert_eq!(h.take_poisoned(), vec!["bad"]);
        assert_eq!(h.page_count(), 1);
    }

    #[test]
    fn visibility_follows_the_view_and_loads_lazily() {
        let (la, lb) = (
            Rc::new(RefCell::new(Log::default())),
            Rc::new(RefCell::new(Log::default())),
        );
        let mut h = host_with(vec![Fake::new("a", &la), Fake::new("b", &lb)]);
        h.set_view(None);
        assert!(
            la.borrow().visibility.is_empty(),
            "nothing shown, nothing told"
        );
        h.set_view(Some(1));
        assert_eq!(lb.borrow().visibility, vec![Visibility::Expanded]);
        assert!(
            la.borrow().visibility.is_empty(),
            "module a was never visible, so it was never loaded"
        );
        h.set_view(Some(0));
        assert_eq!(la.borrow().visibility, vec![Visibility::Expanded]);
        assert_eq!(
            lb.borrow().visibility,
            vec![Visibility::Expanded, Visibility::Hidden]
        );
        h.set_view(Some(0));
        assert_eq!(
            la.borrow().visibility.len(),
            1,
            "unchanged view causes no calls"
        );
    }

    #[test]
    fn polling_happens_only_while_visible_and_expanded() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut m = Fake::new("stats", &l);
        m.poll = Some(Duration::from_secs(1));
        let mut h = host_with(vec![m]);
        assert_eq!(h.next_deadline(), None, "hidden: no poll scheduled");
        h.set_context(10.0, Env::default());
        h.tick();
        assert_eq!(l.borrow().polls, 0);

        h.set_view(Some(0));
        assert_eq!(
            h.next_deadline(),
            Some(10.0),
            "first poll is immediate on becoming visible"
        );
        h.tick();
        assert_eq!(l.borrow().polls, 1);
        assert_eq!(h.next_deadline(), Some(11.0));
        h.set_context(10.5, Env::default());
        h.tick();
        assert_eq!(l.borrow().polls, 1, "not yet due");
        h.set_context(11.2, Env::default());
        h.tick();
        assert_eq!(l.borrow().polls, 2);

        // Collapse: polling stops completely, even though time passes.
        h.set_view(None);
        for t in [12.0, 20.0, 29.0] {
            h.set_context(t, Env::default());
            h.tick();
        }
        assert_eq!(l.borrow().polls, 2, "no polling while collapsed");
        assert!(h.take_out().redraw, "polls can ask for a redraw");
    }

    #[test]
    fn hidden_modules_unload_after_a_while_and_reload_on_demand() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut h = host_with(vec![Fake::new("m", &l)]);
        h.set_context(0.0, Env::default());
        h.set_view(Some(0));
        h.set_view(None);
        assert_eq!(h.next_deadline(), Some(UNLOAD_AFTER));
        h.set_context(UNLOAD_AFTER - 1.0, Env::default());
        h.tick();
        assert_eq!(l.borrow().unloads, 0);
        h.set_context(UNLOAD_AFTER + 0.1, Env::default());
        h.tick();
        assert_eq!(l.borrow().unloads, 1);
        h.tick();
        assert_eq!(l.borrow().unloads, 1, "only once");
        assert_eq!(h.next_deadline(), None);
        h.set_view(Some(0));
        assert_eq!(
            l.borrow().visibility.last(),
            Some(&Visibility::Expanded),
            "it is told again when needed"
        );
    }

    #[test]
    fn wants_frames_only_counts_visible_expanded_modules() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut m = Fake::new("viz", &l);
        m.frames = true;
        let mut h = host_with(vec![m]);
        assert!(!h.wants_frames(), "hidden visualizer costs nothing");
        h.set_view(Some(0));
        assert!(h.wants_frames());
        h.set_view(None);
        assert!(!h.wants_frames());
    }

    #[test]
    fn page_ring_skips_invisible_and_poisoned_modules() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut hidden = Fake::new("media", &l);
        hidden.visible = false;
        let mut big = Fake::new("big", &l);
        big.size = Size::new(400.0, 200.0);
        let h = host_with(vec![hidden, Fake::new("clock", &l), big]);
        assert_eq!(h.page_ids(), vec!["clock", "big"]);
        assert_eq!(
            h.pages(),
            vec![Size::new(300.0, 100.0), Size::new(400.0, 200.0)]
        );
        assert_eq!(h.page_of("big"), Some(1));
        assert_eq!(h.page_of("media"), None);
    }

    #[test]
    fn input_is_routed_to_the_page_owner_with_the_hit() {
        let (la, lb) = (
            Rc::new(RefCell::new(Log::default())),
            Rc::new(RefCell::new(Log::default())),
        );
        let mut h = host_with(vec![Fake::new("a", &la), Fake::new("b", &lb)]);
        assert!(h.input(1, Some(HitId(3)), &Input::Leave));
        assert!(!h.input(0, None, &Input::Leave));
        assert_eq!(lb.borrow().inputs, vec![Some(HitId(3))]);
        assert_eq!(la.borrow().inputs, vec![None]);
        assert!(
            !h.input(9, None, &Input::Leave),
            "out-of-range page is ignored"
        );
    }

    #[test]
    fn drawing_a_page_registers_the_modules_hit_regions() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut h = host_with(vec![Fake::new("a", &l)]);
        let theme = Theme::default();
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &theme);
            h.draw_page(0, &mut cv, Rect::new(10.0, 10.0, 100.0, 40.0));
        }
        assert_eq!(list.hits.len(), 1);
        assert_eq!(list.hit_test(Vec2::new(50.0, 30.0)).unwrap().id, HitId(7));
    }

    #[test]
    fn chips_take_the_two_highest_priority_modules_in_page_order() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mk = |id, w, p| {
            let mut f = Fake::new(id, &l);
            f.chip = Some(w);
            f.prio = p;
            f
        };
        let h = host_with(vec![
            mk("timer", 40.0, 5),
            mk("mic", 20.0, 9),
            mk("media", 60.0, 1),
            Fake::new("plain", &l),
        ]);
        let slots = h.chip_slots();
        assert_eq!(
            slots.iter().map(|s| s.0).collect::<Vec<_>>(),
            vec![0, 1],
            "timer and mic win; shown in page order"
        );
        assert_eq!(h.chips_width(), 40.0 + 20.0 + CHIP_GAP + 2.0 * CHIP_PAD);
        let none = host_with(vec![Fake::new("plain", &l)]);
        assert_eq!(none.chips_width(), 0.0);
    }

    #[test]
    fn chips_are_laid_out_side_by_side_and_collapsed_visibility_is_reported() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut a = Fake::new("a", &l);
        a.chip = Some(30.0);
        let mut b = Fake::new("b", &l);
        b.chip = Some(50.0);
        let mut h = host_with(vec![a, b]);
        h.set_view(None);
        assert_eq!(
            l.borrow().visibility,
            vec![Visibility::Collapsed, Visibility::Collapsed]
        );
        let theme = Theme::default();
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &theme);
            h.draw_chips(&mut cv, Rect::new(0.0, 0.0, 120.0, 20.0));
        }
        let xs: Vec<f32> = list
            .cmds
            .iter()
            .filter_map(|c| {
                if let crate::draw::DrawCmd::Text { rect, .. } = c {
                    Some(rect.x)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(xs.len(), 2);
        // Total = 30 + 10 + 50 = 90, centred in 120 => starts at 15.
        assert!(
            (xs[0] - 15.0).abs() < 1e-3 && (xs[1] - 55.0).abs() < 1e-3,
            "{xs:?}"
        );
        // Expanding hides the chips again.
        h.set_view(Some(0));
        assert_eq!(l.borrow().visibility.last(), Some(&Visibility::Hidden));
    }

    #[test]
    fn config_changes_enable_disable_and_reorder_without_losing_state() {
        thread_local! { static ENABLED: RefCell<bool> = const { RefCell::new(true) }; }
        fn make(_: &Config) -> Option<Box<dyn Module>> {
            ENABLED.with(|e| *e.borrow()).then(|| {
                Box::new(Fake::new("x", &Rc::new(RefCell::new(Log::default())))) as Box<dyn Module>
            })
        }
        fn make_y(_: &Config) -> Option<Box<dyn Module>> {
            Some(Box::new(Fake::new(
                "y",
                &Rc::new(RefCell::new(Log::default())),
            )))
        }
        let f = vec![
            Factory {
                id: "x",
                create: make,
            },
            Factory {
                id: "y",
                create: make_y,
            },
        ];
        let mut h = ModuleHost::new(f, cfg_with(&["x", "y"]), Theme::default());
        assert_eq!(h.module_ids(), vec!["x", "y"]);
        h.apply_config(cfg_with(&["y", "x"]));
        assert_eq!(h.module_ids(), vec!["y", "x"], "reordered");
        ENABLED.with(|e| *e.borrow_mut() = false);
        h.apply_config(cfg_with(&["y", "x"]));
        assert_eq!(h.module_ids(), vec!["y"], "x disabled by config is dropped");
        h.apply_config(cfg_with(&["y", "y", "y"]));
        assert_eq!(h.module_ids(), vec!["y"], "duplicates ignored");
    }

    #[test]
    fn suspend_and_resume_reach_every_module() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut h = host_with(vec![Fake::new("a", &l), Fake::new("b", &l)]);
        h.suspend();
        assert_eq!(l.borrow().suspended, 2);
    }

    #[test]
    fn peek_and_expand_requests_are_collected() {
        let l = Rc::new(RefCell::new(Log::default()));
        let mut m = Fake::new("n", &l);
        m.subs = EventMask::of(&[crate::events::Kind::Battery]);
        struct Asker(Fake);
        impl Module for Asker {
            fn id(&self) -> ModuleId {
                "asker"
            }
            fn title(&self) -> &'static str {
                "asker"
            }
            fn icon(&self) -> Icon {
                Icon::Clock
            }
            fn subscriptions(&self) -> EventMask {
                EventMask::of(&[crate::events::Kind::Battery])
            }
            fn expanded_size(&self) -> Size {
                self.0.size
            }
            fn on_event(&mut self, _ev: &Event, cx: &mut Cx) {
                cx.peek(3.0);
                cx.request_expand();
                cx.redraw_at(5.0);
                cx.redraw_at(2.0);
                cx.command(Command::OpenUrl("https://example.com".into()));
            }
            fn draw_expanded(&mut self, _: &mut Canvas, _: Rect, _: &DrawCx) {}
        }
        let mut h = host_with(vec![]);
        h.entries.push(Entry {
            id: "asker",
            module: Box::new(Asker(m)),
            vis: Visibility::Hidden,
            hidden_since: None,
            loaded: false,
            poisoned: false,
            next_poll: None,
            started: false,
            last_tick: f64::NEG_INFINITY,
        });
        h.dispatch(vec![ev(
            Source::Local,
            EventKind::Battery(BatteryInfo {
                percent: 9,
                charging: false,
            }),
        )]);
        let out = h.take_out();
        assert_eq!(
            out.shell,
            vec![
                ShellRequest::Peek {
                    module: "asker",
                    duration: 3.0
                },
                ShellRequest::Expand { module: "asker" }
            ]
        );
        assert_eq!(out.redraw_at, Some(2.0), "earliest redraw wins");
        assert_eq!(
            out.commands,
            vec![Command::OpenUrl("https://example.com".into())]
        );
        assert!(h.take_out().commands.is_empty(), "taking clears");
    }
}
