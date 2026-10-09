# Shark Notch — design proposal

A tiny, animated, top-centre overlay for Windows (the MacBook-notch / Dynamic-Island idea), written in
Rust with the `windows` crate. No WebView, no Electron, no Tauri, no browser engine.

This document is the "propose first" step. It lists the structure, the crates, the decisions I made
(and which of them you may want to veto), and the honest limits of what could be verified while
writing it. Work proceeds phase by phase as requested; each phase is one or more commits and the
measurements for it are recorded in [`PERFORMANCE.md`](PERFORMANCE.md).

> **Read §9 too.** The proposal below is kept as written, so you can see what was promised. Where the
> build did something different (and why), §9 says so; where it did not manage something, §9 says
> that too.

---

## 0. Decisions you may want to weigh in on

Everything below has a default and the build continues with it. Veto any of them and I will change it.

| # | Decision | My default | Why / what it costs |
|---|----------|-----------|---------------------|
| D1 | Render stack | **Direct2D 1.1 + DirectComposition + DirectWrite, exactly as you specified** | Nothing is *clearly* better; see §3. Refinements: flip-model composition swap chain, frame-latency waitable object, fixed-size back buffer (never `ResizeBuffers` mid-animation). |
| D2 | GPU residency | GPU stack stays warm while you are active, is **released after 5 min of notch inactivity** (configurable), and while a game/fullscreen app is foreground. While released, the collapsed pill is a tiny CPU-rendered layered window (no D3D/D2D/DWrite loaded). Cursor approach pre-warms the GPU stack, so hover still animates immediately. | The GPU driver's user-mode DLLs dominate resident memory. Cost: after a long idle, the *hotkey* path pays a one-off ~tens-of-ms warm-up (hover does not, thanks to pre-warm). `gpu_idle_release_secs = 0` keeps it always warm. |
| D3 | Hover sensing | A **10 Hz coalesced `GetCursorPos` sampler** (≈µs per tick) that is *off* while paused / fullscreen / locked / display-off. | You asked for "event-driven everywhere" **and** a click-through pill **and** no hooks. Windows has no event for "cursor entered a region" that satisfies all three: a window that receives hover also receives (and steals) clicks; `WH_MOUSE_LL` and Raw-Input sinks are the hook-style mechanisms you ruled out. Alternatives in §5.7. This is the one documented exception to "no polling". |
| D4 | Typing into the to-do list | Clicking the to-do input **temporarily activates** the notch (user-initiated), then hands focus back to the previous window on Enter/Esc. Everything else keeps `WS_EX_NOACTIVATE`. | You cannot receive `WM_CHAR` without focus. Toggle: `todo.inline_edit = false` (tasks then come from config / phone / clipboard). |
| D5 | Windows notifications | `UserNotificationListener`, which **requires package identity**. I ship a sparse-MSIX identity (manifest + scripts) and the module degrades to a clear "identity not registered" state without it. | There is no supported unpackaged route. Details and the rejected hacks (UIA scraping, reading `wpndatabase.db`) in §6. **Not testable by me** — needs your machine. |
| D6 | Calendar source | **ICS feeds** (URL or file): Outlook/Google/iCloud all publish them. Join button parses Zoom/Teams/Meet/Webex links from location/description/url. | The Windows `AppointmentStore` also needs package identity and does not cover Google/iCloud accounts. Can be added later behind the same module. |
| D7 | Visualizer | Driven by **`IAudioMeterInformation` peak levels** (no loopback capture, no FFT), sampled at 30 Hz only while *playing AND expanded AND the media page is showing*. | A real spectrum needs loopback capture + FFT (a continuously running audio thread). Peak-driven bars cost almost nothing. A spectrum mode can be added as an opt-in later. |
| D8 | Download / copy progress | **Downloads:** folder watcher on `*.crdownload` / `*.part` / `*.download` (size + speed, indeterminate when total unknown). **Own operations** (file-shelf copies, phone uploads): real progress. **Explorer copy:** *not feasible* without injecting into Explorer or scraping UIA, so it is skipped. | You said "where feasible"; this is where the line is. |
| D9 | iPhone transport | Hand-written HTTP/1.1 server on `std::net`, **plain HTTP + bearer token**, binds only to private-range interface addresses, rejects non-private peers, per-IP auth-failure lockout, streaming uploads to disk with a size cap. No TLS. | iOS Shortcuts cannot reasonably pin a self-signed cert. The token travels in clear on your LAN; the docs say so and recommend your home network only. |
| D10 | Config / data | `%APPDATA%\SharkNotch\config.toml` (hot reloaded). Data (clips, shelf index, to-dos) in `%LOCALAPPDATA%\SharkNotch\`. | Standard locations; config is user-editable, data is not. |
| D11 | Toolchain | Rust stable (edition 2024), `windows` 0.62, MSVC target `x86_64-pc-windows-msvc`. | |
| D12 | Licence | **None added.** | Not my call. Bloom is GPL-3.0; nothing here is derived from it (see §1). |

---

## 1. Ground rules

* **Bloom is inspiration only.** I read its docs and the shape of its UX (which pages exist, how scroll
  cycles them with a cooldown, the *feel* of its spring constants, sizes of its panels) and nothing
  else. No code, CSS, assets, identifiers or layouts were copied; everything here is written from
  scratch. Where Bloom is heavy (WebView2, global keyboard hook, an always-on visualizer, a taskbar
  replacement) this project does the opposite.
* **Never** inject into other processes, **never** install low-level keyboard/mouse hooks, **never**
  use `SetWindowsHookEx` at all. The only global input mechanism is `RegisterHotKey`.
  `SetWinEventHook` is used **out-of-context** (`WINEVENT_OUTOFCONTEXT`, no DLL injection) only for
  foreground/fullscreen tracking, as you specified.
* **Budget:** ~10–30 MB RAM and ~0 % CPU at idle; render loop only during animation; everything
  event-driven except the documented sampler (D3) and visible-and-expanded polling.

## 2. How this was verified (read this)

The work was done in a Linux container; Windows binaries cannot run here. So there are three tiers of
confidence, and every claim in the docs says which tier it is in:

1. **Unit-tested natively** — all logic that does not touch Win32 lives in `notch-core`
   (`#![forbid(unsafe_code)]`): springs, frame pacing maths, event bus, module host, hover/shell state
   machines, config, display lists, layout of every module's views, ICS/recurrence, pomodoro, clipboard
   history, the HTTP listener and its auth, stats ring buffers. These run with `cargo test` on Linux
   *and* on Windows CI.
2. **Compiled against the real Windows API surface** — `shark-notch` is type-checked with
   `cargo check --target x86_64-pc-windows-msvc` on every change, and built + tested on a
   `windows-latest` GitHub Actions runner (see `.github/workflows/ci.yml`), where a `--selftest`
   mode creates the real window / D3D / D2D / DirectComposition stack, runs scripted animations,
   and prints frame-time and memory/CPU numbers into the job log.
3. **Needs your hardware** — anything that depends on a real GPU driver, your monitors, your phone,
   package identity, anti-cheat behaviour, or taste. CI runners have no GPU (WARP software rasteriser),
   so absolute RAM numbers from CI are *not* your numbers. `shark-notch --selftest` and the tray
   "Diagnostics" item print the same report on your machine; that is the number to trust.

A dev-only crate (`notch-preview`) renders any module's display list to PNG on any OS, so layouts were
inspected visually even though the Direct2D backend could not be run locally.

## 3. Rendering: why D2D + DirectComposition + DirectWrite stays

Considered and rejected:

* **Windows.UI.Composition (WinRT) spring animations on the compositor.** Zero CPU in our process
  during motion, which is attractive. Rejected: shapes are limited (one uniform corner radius, no
  continuous corners/ears without path re-set), velocity-preserving retargeting is not guaranteed,
  it needs a DispatcherQueue and extra WinRT DLLs (RAM), and — decisively — it makes the
  "record frame times and treat any hitch as a bug" requirement unmeasurable because we would not
  be producing the frames.
* **wgpu / Skia / softbuffer + tiny-skia.** wgpu pulls a Vulkan/DX12 loader (+10–20 MB). Skia is a
  large native dependency. CPU compositing cannot be vsync-locked to DWM without tearing/judder.
* **GDI layered windows (`UpdateLayeredWindow`)** for everything: not vsync-synchronised, copies the
  whole bitmap every frame. Used *only* for the static idle pill (D2).

What the chosen stack looks like:

```
D3D11 device (hardware, WARP fallback) ─ DXGI device ─ D2D1 device ─ D2D device context ─ DirectWrite
                      │
      IDXGISwapChain1 (composition, flip-sequential, premultiplied alpha, 2 buffers,
                       FRAME_LATENCY_WAITABLE_OBJECT, max latency 1, FIXED size)
                      │
      IDCompositionDevice ─ IDCompositionTarget(HWND) ─ IDCompositionVisual(swap chain)
```

* The HWND (`WS_EX_NOREDIRECTIONBITMAP | LAYERED | TRANSPARENT | NOACTIVATE | TOOLWINDOW | TOPMOST`)
  and the swap chain are sized once for the largest panel; shapes animate *inside* them. No
  per-frame `SetWindowPos`/`ResizeBuffers`, so no hitches from either. Hit-testing is narrowed with
  `SetWindowRgn` only at settled states; while collapsed the whole window is click-through.
* Frames are paced by the swap chain's waitable object (`SetMaximumFrameLatency(1)`), integrated
  into a single `MsgWaitForMultipleObjectsEx` loop. When nothing is animating the loop waits with
  an `INFINITE`/next-deadline timeout: 0 % CPU.
* The animation clock is the **compositor's vblank timeline** (`DwmGetCompositionTimingInfo`),
  quantised to the refresh grid, not "now": sampling at the scheduled display time removes wake-up
  jitter (±0.5 ms × 3000 px/s ≈ 1.5 px of judder otherwise). Dropped vblanks advance the spring by
  the real elapsed time, so motion stays correct.
* Springs use the **closed-form** damped-oscillator solution, so any `dt` (including a dropped
  frame) is exact, and retargeting mid-flight keeps position *and* velocity.
* Corners are continuous (G2) curves built from two cubic Béziers around a circular arc; the
  construction is unit-tested for tangent and curvature continuity. Notch "ears" (the concave
  fillets where a hardware notch meets the menu bar) use the same function.
* Text is DirectWrite with `Segoe UI Variable` (falls back to `Segoe UI`), grayscale AA (ClearType is
  not available on a transparent target), `D2D1_DRAW_TEXT_OPTIONS_NO_SNAP` so scaling text during
  a transition does not shimmer. Icons are vector paths built in `notch-core` (no font dependency,
  crisp at any DPI, and they show up in the PNG previews).

## 4. Workspace and crates

```
crates/
  notch-core/     portable, #![forbid(unsafe_code)], all logic + tests
  shark-notch/    Windows binary: window/GPU/OS glue only
  notch-preview/  dev tool: display list -> PNG (tiny-skia + fontdue), not shipped
docs/             DESIGN, PERFORMANCE, IPHONE_SHORTCUTS, NOTIFICATIONS
packaging/        sparse-package manifest + registration scripts
.github/          CI (Linux tests, Windows build/test/selftest)
```

| Crate | Where | Why it earns its place |
|-------|-------|------------------------|
| `windows` 0.62 | shark-notch | Official bindings for every Win32/WinRT API used. Feature list is explicit and minimal. |
| `serde` + `toml` | core | Config parsing with good error messages. Without default features beyond `parse`/`serde`. |
| `serde_json` | core | Phone API bodies are *untrusted input*: a real parser beats a hand-rolled one. Also used for the tiny persisted stores. |
| `tiny-skia`, `fontdue` | notch-preview only | Visual verification of layouts on Linux. Never linked into the app. |

Deliberately **not** used: any async runtime, HTTP/TLS stack (WinHTTP via `windows` handles the one
outbound HTTPS need: ICS fetching), logging framework (a 60-line file logger), chrono/time (civil-date
maths is ~80 tested lines; Win32 supplies the local time zone), `anyhow`, `once_cell`, `bitflags`.

## 5. Architecture

### 5.1 Threads

* **UI thread (STA)** — owns every window, all D3D/D2D/DComp objects, OLE (drag/drop), the shell
  state machine, the module host and the event dispatch. All rendering happens here.
* **Workers (short-lived or parked)** — every blocking call: WinRT `.get()`, clipboard reads, file I/O,
  WinHTTP, PDH, registry enumeration, the HTTP listener. A worker only ever talks to the UI thread by
  `BusSender::send(Event)` (coalesced wake-up `PostMessage`). No locks are shared with the render path.
* WinRT/SMTC event callbacks arrive on thread-pool threads and just forward to the bus.

### 5.2 Event bus

```rust
pub enum Source { Local, Phone }
pub struct Event { pub source: Source, pub at: Instant, pub kind: EventKind }
pub enum EventKind { ClipboardItem(..), Notification(..), FileDropped(..), Battery(..),   // Battery+Phone == "PhoneBattery"
                     FocusChanged(..), MediaChanged(..), ActivityStarted/Progress/Ended(..), ... }
```

Producers clone a `Send` handle; the UI thread drains an `mpsc` queue when woken and dispatches to
subscribers by `EventMask` (a `u64` bit set from the kind's discriminant). Payloads are small (ids,
`Arc<str>`, numbers); bulky data (album art, images) is referenced by id into a cache. Because `source`
is data, the UI renders local and phone events through the same code path.
Commands travel the other way (`Command::Media(PlayPause)`, `Command::CopyClip(id)`, …) so modules
never call Win32 directly and stay unit-testable.

### 5.3 Module trait and display lists

A module is *logic + layout*, never pixels. It implements:

```rust
trait Module {
    fn id(&self) -> ModuleId;
    fn subscriptions(&self) -> EventMask;
    fn poll_interval(&self) -> Option<Duration>;     // honoured ONLY while visible && expanded
    fn wants_frames(&self) -> bool;                  // continuous animation (e.g. visualizer)
    fn on_event(&mut self, e: &Event, cx: &mut Cx);
    fn on_poll(&mut self, cx: &mut Cx);
    fn on_visibility(&mut self, v: Visibility, cx: &mut Cx);   // lazy load / unload resources
    fn on_input(&mut self, hit: Option<HitId>, i: &Input, cx: &mut Cx) -> bool;
    fn on_suspend(&mut self, cx: &mut Cx); fn on_resume(&mut self, cx: &mut Cx);
    fn chip(&self) -> Option<Chip>;                  // contribution to the collapsed pill
    fn expanded_size(&self) -> Size;  fn peek_size(&self) -> Option<Size>;
    fn draw_chip/draw_expanded/draw_peek(&mut self, cv: &mut Canvas, area: Rect, t: f32);
}
```

`Canvas` emits a `DrawList` (rounded rects, paths, arcs, text, icons, images, clips, alpha/scale
groups) plus hit regions. The Direct2D backend executes the list; the preview tool executes the same
list with tiny-skia. Hit-testing, hover, drag and cursor shape come from the hit regions, so modules
are fully testable ("click at the play button's centre → `Command::Media(PlayPause)`").
Modules are compiled in but **instantiated only when enabled** and **initialise heavy resources
(WinRT, audio, PDH, WIC) only on first visibility**, dropping them after they have been hidden for a
while. Panics inside a module are caught at the host boundary; the module is disabled and logged,
the notch keeps running.

### 5.4 Shell state machine and springs

`Shell` is a pure, time-injected state machine: `Hidden | Collapsed | Peek | Expanded`, with reasons
(hover, hotkey, attention, drag-to-shelf). Input: hover-sampler results, hotkey, module attention
requests, fullscreen/lock/pause flags, `now`. Output: *targets* (not frames) for independent springs —
`width`, `height`, `radius_top`, `radius_bottom`, `ear`, `y` (each with its own frequency/damping),
plus `content` (0–1) for the stagger — and window state (click-through or not, region, visibility).
**Stagger:** on expand the shape springs first and the content spring is armed ~70 ms later (fade +
scale 0.96→1); on collapse content clears first, the shape follows ~40 ms later. Switching pages
cross-fades outgoing/incoming content while the shape retargets (velocity preserved).
**Reduce motion** (`SPI_GETCLIENTAREAANIMATION` off, or config): targets are applied with a short
linear fade instead of springs.

### 5.5 Frame loop and pacing

```
loop {
    timeout = if animating { INFINITE } else { scheduler.next_deadline() or INFINITE }
    MsgWaitForMultipleObjectsEx([frame_waitable if animating], timeout, QS_ALLINPUT)
    -> due timers (hover tick, dwell, collapse delay, module polls, auto-peek hide, idle release)
    -> drain bus -> dispatch; pump window messages
    -> if frame signalled: step springs to the vblank sample time, build display list, draw, Present(1)
}
```
First frame of an animation is rendered synchronously in the handler that received the trigger
(within one frame of the event). A `FrameRecorder` keeps per-burst frame intervals/CPU time; a burst
report (frames, p50/p95/p99/max, count over 1.5× refresh interval) is logged and kept for the tray
"Copy frame report" item. Any burst with a hitch is flagged `HITCH` in the log and in CI output.

### 5.6 GPU residency (D2)

`Gfx` is `Option<GpuStack>`. States: `Released → Warming → Warm`. Warm-up is triggered by cursor
*approach* (within ~250 px of the zone), hotkey, attention event, or drag-to-top; creation is
synchronous on the UI thread when nothing is animating (a few tens of ms, off the critical path
because it starts before the dwell completes). Release destroys the stage HWND, swap chain,
D2D/D3D/DComp objects, calls `EmptyWorkingSet`, and shows the CPU-rendered pill window. Device-lost
(`DXGI_ERROR_DEVICE_REMOVED/RESET`) takes the same Released→Warm path. Fullscreen/lock/display-off force Released.
Handover pill↔stage happens only at idle boundaries (pixel-identical shapes, rendered first, then
shown, then the old one hidden), never mid-animation.

### 5.7 Hover, hotkey, drag-to-shelf — without hooks

* **Hover:** `HoverSampler` ticks at 10 Hz (`SetWaitableTimerEx`-style tolerance via the loop
  timeout; 30–60 Hz only while the cursor is inside the *approach/zone* rectangles). It expands
  after a ~150 ms dwell inside a *narrow top-centre zone* (default 200×6 DIP at the monitor's top
  edge — not the visible pill), unless a mouse button is down (`GetAsyncKeyState`), a
  keystroke happened recently (`GetLastInputInfo` advancing while the cursor has not moved — i.e.
  non-pointer input; no key logging, no hook), a move/size loop is active (`GetGUIThreadInfo`), or
  the shell is paused/suspended. While expanded, the keep-open region is the panel plus margin and
  collapse has a short grace delay.
* **Why not the alternatives:** an invisible 1-px hit-testable strip would steal the top pixel row
  (Chromium's Fitts-law tab hit area); raw-input sinks and `WH_MOUSE_LL` are the hook-style mechanisms
  you excluded; `EVENT_OBJECT_LOCATIONCHANGE` is not delivered for the cursor.
* **Hotkey:** `RegisterHotKey` only (toggle; optional "peek" off by default).
* **Drag-to-shelf:** while the sampler sees the left button down with the cursor approaching the
  top-centre, a UI-thread capture check (`GetGUIThreadInfo.hwndCapture` + cursor shape + not in
  move/size) arms the shelf: the window stops being click-through, expands to the shelf size, and
  the real `IDropTarget::DragEnter` confirms (or, after ~400 ms with no `DragEnter`, it disarms).
  This is a heuristic; the OLE callbacks are the ground truth.

### 5.8 Fullscreen / game policy

* `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` (out-of-context, skip-own-process) for foreground
  changes, plus an `EVENT_OBJECT_LOCATIONCHANGE` hook **scoped to the foreground window's
  process/thread** (re-registered per foreground change) so F11-style transitions are caught;
  debounced 120 ms.
* Decision: window rect covers its monitor rect **and** is not a shell window **and** has no caption
  → fullscreen. `SHQueryUserNotificationState` is the secondary signal (`D3D_FULL_SCREEN` or
  `PRESENTATION_MODE` force fullscreen; `BUSY`/`APP` alone do not, they are too eager).
* On fullscreen: hide the window, `Module::on_suspend` for all modules, release the GPU stack,
  disable hover/drag sensing (sampler stops), `EmptyWorkingSet`. Event-source threads that are
  *cheap and passive* (clipboard listener, notification listener, phone listener) keep queueing;
  notifications are counted and a "missed" dot is shown after the game ends. Optional `peek` hotkey
  (off by default) shows the notch briefly over borderless-fullscreen.
* Never touched: other processes' memory, handles (we do not `OpenProcess` any foreground window),
  input streams.
* Also suspends on session lock (`WTSRegisterSessionNotification`), display off
  (`GUID_CONSOLE_DISPLAY_STATE`), and user "pause" from the tray.

### 5.9 Config

`serde` structs with `#[serde(default)]` everywhere, validated/clamped after parsing (`Config::validate`
returns warnings). A directory change notification (`FindFirstChangeNotification`, debounced 250 ms)
reloads; a parse error keeps the previous config and surfaces the message in the tray tooltip and
log. A commented default file is written on first run.

## 6. Phase plan and feasibility notes

| Phase | Deliverable | Feasibility notes |
|------:|-------------|-------------------|
| 1 | Shell | Fully feasible. Hover is sampled (D3). Needs your eyes for taste. |
| 2 | Bus + `Module` + clock | Feasible; pure logic, fully tested. |
| 3 | Media | SMTC works for unpackaged Win32. Album art decoded by WIC straight to ≤128 px. Visualizer = peak meter (D7). |
| 4 | Clipboard | `AddClipboardFormatListener`. Honours `ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory` (password managers). Images stored as PNG on disk, only thumbnails in RAM. |
| 5 | File shelf | OLE `IDropTarget`/`IDropSource`, `SHCreateDataObject` for drag-out. v1 handles `CF_HDROP`; virtual files (`FILEDESCRIPTOR`) are a follow-up. |
| 6 | Notifications | **Requires package identity**. Sparse MSIX: manifest + `register-identity.ps1` (Developer-Mode loose registration, or signed package with a self-signed cert). `NotificationChanged` is documented as unreliable for desktop apps, so a fallback re-query after foreground changes is included but off by default. Rejected: UIA scraping of toasts (fragile, injects UIAutomationCore into other processes) and reading `wpndatabase.db` (undocumented schema + SQLite dependency). **Untestable by me.** |
| 7 | Calendar + Pomodoro + to-do | ICS feeds with RRULE expansion + `VTIMEZONE` evaluation; WinHTTP for fetch. Join button = `ShellExecute`. |
| 8 | Live activities | Mic/camera via `RegNotifyChangeKeyValue` on `CapabilityAccessManager\ConsentStore`. Timers. Downloads via folder watcher. Explorer copy progress **not feasible**. |
| 9 | System stats | `GetSystemTimes`, `GlobalMemoryStatusEx`, `GetIfTable2`, PDH for GPU engine utilisation (lazy, torn down after hidden). Battery is event-driven via power-setting notifications. Polled **only** while visible and expanded. |
| 10 | Command center | Volume = `IAudioEndpointVolume` (+ change callback only while visible); brightness = `\\.\LCD` IOCTL for internal panels, DDC/CI (`dxva2`) for externals; Wi-Fi/Bluetooth = `Windows.Devices.Radios`; DND = `NOC_GLOBAL_SETTING_TOASTS_ENABLED` (Windows 11; version dependent, best-effort); screenshot/snip = `ms-screenclip:` / `SendInput`. |
| 11 | iPhone listener | Hand-written HTTP/1.1 (D9). Endpoints: `GET /v1/ping`, `POST /v1/clipboard`, `POST /v1/file`, `POST /v1/battery`, `POST /v1/focus`. Auth tests, size/timeout/lockout tests. `docs/IPHONE_SHORTCUTS.md`. |

## 7. Performance budget and how it is measured

| Metric | Budget | How |
|--------|--------|-----|
| Idle private working set, GPU released | ≈ 5–10 MB | `--selftest` prints `GetProcessMemoryInfoEx.PrivateUsage`/`WorkingSetSize` in each state. |
| Idle private working set, GPU warm | ≤ 30 MB target; driver-dependent | same; CI (WARP) is *indicative only* |
| Idle CPU | ≈ 0 % (sampler: ~10 wakeups/s, µs each) | `GetProcessTimes` over a 20 s idle window |
| Animation CPU | low single-digit ms/frame, no allocations in the steady-state frame | `FrameRecorder` CPU time per frame |
| Hitches | zero frames > 1.5× refresh interval | `FrameRecorder` report; CI fails the job if the scripted run records one *and* the runner is not WARP-throttled (WARP runs are reported, not gated) |

`docs/PERFORMANCE.md` is appended to after every phase with what was measured, where (Linux unit
tests / Windows CI / needs-your-machine), and what changed.

## 8. Risks and things I cannot settle from here

* DirectComposition quirks (first-frame flash, region clipping, `WS_EX_LAYERED` + DComp
  combination) — mitigated by rendering and committing the first frame before showing, and exercised
  by the CI selftest, but real GPUs/DPIs/multi-monitor setups are yours to confirm.
* Whether `UserNotificationListener.NotificationChanged` fires for a sparse-package desktop app on your
  Windows build.
* DND toggle registry semantics and brightness IOCTL support vary by Windows build / panel driver.
* Anti-cheat: nothing here uses hooks, injection or foreign-process handles, and everything is
  suspended while a fullscreen app is foreground — but I cannot test against EAC/BattlEye/Vanguard.

## 9. What changed while building

The proposal above was followed except where listed here. Nothing in this list is hidden elsewhere:
if the build deviated, it is here.

**Done differently**


* **Windows notifications (D5, phase 6) were removed again.** Reading other apps' toasts needs a
  package identity, which never worked on the target machine, so the listener, the sparse-package
  manifest and scripts (`packaging/`) and `docs/NOTIFICATIONS.md` are gone. The Notifications page
  stays for the banners the iPhone link sends. The application manifest (segment heap) moved to
  `crates/shark-notch/shark-notch.exe.manifest`.
* **The iPhone page was removed.** The link, its `/notify` banners and the other endpoints stay; the
  pairing token is in the tray menu. The phone's battery and Focus are still accepted and shown
  nowhere.
* **iPhone link (D9, phase 11).** It listens on **all IPv4 addresses** (`0.0.0.0`), not "only
  private-range interface addresses": who may talk to it is decided *per connection* (a private
  address, and by default one on the same network as one of the PC's own adapters; anything else is
  dropped before a byte is read). Following the network as adapters come and go is more machinery
  than that check, for no more safety. The paths are `/ping`, `/clipboard`, `/file`, `/battery`,
  `/focus` and `/notify` (no `/v1/` prefix; `/notify` is new). Added beyond the proposal: the token is
  checked **before** the path is routed (a stranger learns nothing, not even which paths exist),
  constant-time token comparison, a folder per received file, blocked program and script types, the
  "downloaded from the internet" mark on received files, retention and a cap on the inbox, and
  pacing so that requests from a stranger (who has no token) cannot cause more than one redraw a
  second. The link is **off by default**.
* **Battery (phase 9)** is read with `GetSystemPowerStatus` when the page asks for a reading (while it
  is on screen), not through power-setting notifications. It costs nothing while the page is closed
  either way.
* **Command centre (phase 10).** The volume is read once a second while the page is open instead of
  through a change callback. **Do-not-disturb is read-only:** Windows has no supported way to change
  it (see [`CONTROL.md`](CONTROL.md)). **No DDC/CI**: brightness works for the built-in panel only;
  external monitors say so on the page.
* **Typing into the to-do list (D4).** The `todo.inline_edit` switch was not built. Clicking the input
  activates the notch for the duration of the edit, as proposed; turning the whole Focus page off
  (`[pomodoro] enabled = false`) is the way to have no typing at all.
* **Fullscreen suspension (§5.8).** The media, clipboard and calendar services pause, the
  microphone/camera watcher stops, and the stats and control workers only ever run while their page
  is open. The iPhone listener keeps listening (it is a thread asleep in `accept()`); what it
  receives is queued silently like any other notification.
* **CI and hitches (§7).** CI does **not** fail a run for a hitch: the self-test reports every burst
  and the total, and prints a heartbeat that says whether the whole virtual machine stalled (a
  hitch there is not the app's). It fails on a frame that cannot be drawn or presented, too few
  frames, and any scenario check that does not hold. A gate on hitches would fail on VM noise.
* **`PERFORMANCE.md`** was written at the end, from the CI logs of each phase's run, not appended after
  every phase as §7 says. The numbers are the self-test's own; the per-phase table is in that file.

**Smaller differences in the details** (found by reading every statement of the proposal against the
code at the end; the sections above are left as written)

* **D3 / §5.7, hover.** The sampler also runs at 30 Hz the whole time the notch is expanded or showing
  a banner, and at 60 Hz while a drag is armed (not only inside the approach rectangles). The
  approach margin defaults to 140 DIP around the 200×6 DIP zone, which is about 240 DIP from the
  centre rather than "~250 px". A drag disarms when the button is released or when the cursor stays
  outside the drag zone for 400 ms; `DragEnter` is not consulted for that.
* **D7 / phase 3.** The level meter is read by a small thread about 120 times a second (8 ms sleeps),
  not at 30 Hz; the thread exists only while a track is playing *and* the media page is open. Album
  art is decoded to at most 192 px, not 128.
* **D10.** What is saved in `%LOCALAPPDATA%\SharkNotch\`: pinned clipboard text (`pins.json`; the history
  itself is never written), tasks and the timer (`pomodoro.json`), quick timers (`timers.json`), the
  iPhone token and inbox, and the log. There is no shelf index: the shelf is not persisted.
* **§2, tier 2.** CI builds on a Windows runner; `cargo check --target x86_64-pc-windows-msvc` is what
  was used on the Linux build machine, not a CI step. The tray's *Copy diagnostics* copies a snapshot
  of the session (monitor, refresh, GPU state, memory and CPU now, frame bursts, the last log lines);
  `--selftest` is what runs the measurements and the pass/fail checks.
* **§3.** The window region is set when the notch becomes interactive (the start of an expansion), to
  the destination rectangle plus 6 DIP, not only at settled states.
* **§4.** `docs/` also holds `CALENDAR_AND_FOCUS`, `LIVE`, `STATS` and `CONTROL`. The file logger is 140
  lines and the civil-date maths 160 (plus tests), not 60 and 80.
* **§5.1.** One lock is shared with the render path: the image cache is a `Mutex` that producers fill
  and the renderer reads.
* **§5.2 / §5.3 sketches.** The code sketches show the idea, not the final types. `EventMask` is a `u32`
  bit set over 22 kinds; there are no `ActivityStarted/Progress/Ended` events (Privacy, Downloads and
  DownloadDone are separate kinds); a module has `chip_width` and `chip_priority` rather than a
  `chip()` method. `events.rs` and `module.rs` are the truth.
* **Lazy modules.** Only the stats and command-centre workers and the audio meter are created on their
  first request. For an enabled module, the services behind it (SMTC and WinRT, the clipboard
  listener, notifications, calendar feeds, the microphone watcher, the downloads watcher, the shelf,
  the iPhone listener) start at launch and idle until something happens; they are stopped when the
  module is switched off, and paused while a fullscreen app is in front (the iPhone listener keeps
  listening, as the "Fullscreen suspension" point above says).
* **Phase 5.** Drag-out builds its data object from `SHCreateShellItemArrayFromIDLists` and
  `BHID_DataObject`, not `SHCreateDataObject` (see the comment in `win/dragdrop.rs` for why).
* **Phase 6.** The "fallback re-query after foreground changes" was **not built**: the page re-queries
  when it is opened, and nothing else polls. If `NotificationChanged` turns out to be unreliable on your
  machine, that is the missing piece.
* **Phase 10.** The snip button is `ms-screenclip:` only; no input is synthesised anywhere (there is
  no `SendInput`).
* **§7.** Idle CPU is measured from `QueryProcessCycleTime` over 5 s windows (`GetProcessTimes` is
  printed beside it, as a cross-check, because it only advances in 15 ms ticks); memory is the private
  working set (`PrivateWorkingSetSize`) and the commit (`PrivateUsage`). CI does not gate on hitches
  (see above).
* **§5.8, "a missed dot".** It is a bell with a count (capped at "9+").

**Not done**

* **Progress for the app's own transfers** (D8: shelf copies, phone uploads). Nothing shows a progress
  bar for them. Downloads show size and speed; Explorer copies remain infeasible, as proposed.
* **Phone to tasks / to-do.** The proposal mentioned tasks arriving from the phone; there is no such
  address.
* **A QR code for pairing.** The token is copied to the clipboard and you move it to the phone
  yourself (see [`IPHONE_SHORTCUTS.md`](IPHONE_SHORTCUTS.md)). A QR code on the page would make that
  nicer and would also put the token on the screen; it is a possible addition, not a promise.
* **TLS** for the iPhone link, as decided in D9.
* **A licence file.** D12 stands: none was added.

**Could not be verified here** (the build ran on a Linux container and a virtual Windows runner):
anything on a real GPU, panel, microphone, laptop battery, radios, the Windows notification
listener, a real browser's downloads, real drag and drop, a real iPhone and Shortcuts, a real
Windows Firewall prompt, and anti-cheat software. Each module's document lists its own.

