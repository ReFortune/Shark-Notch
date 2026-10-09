# Shark Notch

A tiny, animated overlay at the top-centre of your Windows screen — the MacBook notch / Dynamic Island
idea — written in **Rust** with the `windows` crate. No Electron, no WebView2, no browser engine:
**Direct2D + DirectComposition + DirectWrite**, one small process, event-driven everywhere it can be.

> **Status.** All eleven planned phases are implemented, and every push is built, tested and put through a
> scripted self-test of the real app on a Windows CI runner. **What nobody has done yet is run
> it on your hardware.** The build machine was a Linux container plus a virtual Windows machine with
> no GPU, no audio device, no battery and no iPhone, so each page below says what that leaves
> unproven. Read [What is and is not verified](#what-is-and-is-not-verified) before you trust a
> claim, [`docs/DESIGN.md`](docs/DESIGN.md) (§9 lists where the build differs from the proposal) and
> [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) for the numbers.

## What it does

The collapsed notch is a thin pill hugging the top edge of your monitor. Hover the top-centre for
~150 ms (or press <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>N</kbd>) and it springs open; scroll or swipe to
switch pages; move away and it springs shut. Small **chips** appear on the pill for things that
matter right now (a microphone in use, a timer, the next meeting, a download, your phone's Focus).

| Page | What it is | More |
|------|-----------|------|
| **Media** | Whatever Windows thinks is playing: art, title, seek bar, controls, level bars; optionally (off by default) the line being sung, from lrclib.net | [`docs/MEDIA.md`](docs/MEDIA.md) |
| **Clipboard** | History of text, links and images; pin; click to copy back; items from your iPhone are labelled | |
| **Shelf** | Drop files on the notch (a drag toward the top opens it), drag them out again. It holds *references*: your files are never moved or deleted | |
| **Notifications** (pop-up only, no page) | A short banner for what your iPhone shortcuts send, and one "while you were away" banner after a fullscreen session. Windows' own notifications are not read (Windows allows that only to apps with package identity) | [`docs/IPHONE_SHORTCUTS.md`](docs/IPHONE_SHORTCUTS.md) |
| **Calendar** | Your ICS feeds (Outlook, Google, iCloud): month grid, agenda, a **Join** button, a countdown chip | [`docs/CALENDAR_AND_FOCUS.md`](docs/CALENDAR_AND_FOCUS.md) |
| **Focus** | Pomodoro timer with a task list and a stopwatch | same |
| **Live** | A chip while a program uses the microphone or camera, quick timers, browser downloads in progress | [`docs/LIVE.md`](docs/LIVE.md) |
| **Stats** | CPU, memory, GPU, network and battery, with a minute of history (measured only while the page is open), connected Bluetooth devices with their batteries, a pop-up when your Claude plan limit is nearly used up (opt-in), and a banner when the charger is plugged in or the battery is full | [`docs/STATS.md`](docs/STATS.md) |
| **Controls** | Volume, brightness, Wi-Fi, Bluetooth, a snip button, Keep awake, microphone mute, Windows' Focus state (read only) | [`docs/CONTROL.md`](docs/CONTROL.md) |
| **iPhone link** (no page) | iOS Shortcuts send clipboard text, files and banners; banners appear as notifications marked "· iPhone". Tray: *Copy iPhone token* / *New iPhone token* | [`docs/IPHONE_SHORTCUTS.md`](docs/IPHONE_SHORTCUTS.md) |
| **Clock** | Time, date, week number | |

Every module can be switched off in `config.toml` (a module that is off is never created: no thread, no
subscription, no loaded WinRT) and reordered in `[modules] order`.

**Premium motion.** Width, height, corner radius, ear size and position are *independent springs*;
every transition is interruptible and keeps its velocity; the shape moves first and the content
follows; frames are vsync-locked and refresh-rate aware; it respects Windows' "Animation effects"
setting.

**Stays out of the way.**

* It does not take focus (`WS_EX_NOACTIVATE`): hovering, clicking its buttons and scrolling leave the
  window you were typing in alone. The one exception is the Focus page's *Add task* field, which takes
  the keyboard until Enter or Esc. It has no taskbar button and no main window — a tray icon only
  (hideable), one instance, optional start with Windows.
* It is **hidden from screenshots and screen sharing** by default (`exclude_from_capture`).
* It **steps aside while a game or any fullscreen app is in front**: the notch hides, the modules
  pause, the GPU is released, hover is off; notifications queue silently and a "missed" badge shows
  afterwards. Detection uses `SetWinEventHook` (out of context) and the monitor's coverage, with
  `SHQueryUserNotificationState` as a second opinion; nothing polls for it.
* **No hooks, no injection.** It never injects into another process and installs no keyboard or mouse
  hook (that is how anti-cheat tells friends from cheats). `RegisterHotKey` is the only global input
  mechanism; hover is a 10 Hz cursor sample that stops entirely while paused, locked, display-off or
  when a fullscreen app is in front.
* **Nothing phones home.** The only network use is the calendar feeds you configure (HTTPS, through
  Windows' own stack) and, **if you switch it on**, the iPhone link on your own network. No telemetry,
  no update check.

## Build and run

You need Windows 10 (2004+) or 11, the Rust stable toolchain (edition 2024, `rustup`), and the MSVC
build tools.

```powershell
cargo build --release -p shark-notch
.\target\release\shark-notch.exe          # runs in the tray; Ctrl+Alt+N toggles the notch
```

If there is no `config.toml` yet, a commented one is written to `%APPDATA%\SharkNotch\` (tray menu →
*Open settings (config.toml)*). Saved changes apply immediately; a mistake keeps the previous
settings and shows the error in the tray tooltip. A `config.toml` that exists but cannot be read
(saved as UTF-16, locked) is never overwritten: the notch runs on the defaults and says so. Logs and
saved data live in `%LOCALAPPDATA%\SharkNotch\` (`notch.log`, pinned clip text, tasks and timers,
the iPhone token and inbox).

The tray menu: open/close the notch, pause, open or reload the settings, start with Windows (written
to `config.toml` as `autostart`, which is what decides at every launch), copy a frame-time report,
copy diagnostics, hide the tray icon, quit.

Command-line switches:

| Switch | Effect |
|--------|--------|
| `--selftest [--out file]` | Scripted run of the real app; prints frame-time bursts, idle CPU/RAM with the GPU warm and released, the GPU warm-up cost, and a pass/fail line for every scenario. Add `--no-exclude --light-probe` to also probe the actual screen pixels, and `--registry-probe` to let it write (and remove) a fake microphone-use record to test the privacy chip. |
| `--config path` | Use a different config file. |
| `--console` | Echo the log to the console that launched it. |

Anything else on the command line is ignored (the *start with Windows* entry passes `--autostart`, which changes nothing).

`--selftest` uses a scratch data folder and its own configuration; it never touches your settings or
saved data. **Run it on your machine** if anything feels off: it prints the numbers that matter for
*your* GPU driver ([`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) says how to read them).

### On other operating systems

The platform-independent core builds and tests anywhere:

```bash
cargo test -p notch-core                 # 500+ tests: springs, shell, hover, config, bus, module host, every module, the phone protocol...
cargo run -p notch-preview -- shell out/shell.png     # render the shell's choreography to a PNG
cargo run -p notch-preview -- modules out/modules.png # ...or the real module host (chips, pages, peek)
cargo check --target x86_64-pc-windows-msvc -p shark-notch   # type-check the Windows app
```

## Architecture (one page)

```
notch-core  (portable, #![forbid(unsafe_code)], all logic + tests)
  spring · path (continuous corners) · shell (state machine) · hover (FSM) · compose
  config · hotkey · input · fullscreen (decision) · frame (pacing) · raster · icons · draw (display list)
  bus (typed events with a source tag) · module (the Module trait + host) · modules/* (one file each)
  the logic of the services: phone (HTTP and auth), clipstore, ics, timers, stats, chart ...

shark-notch (Windows glue only)
  app          the controller + single message loop; GPU residency; suspension; hot reload
  gfx::stack   D3D11 -> DXGI -> D2D -> DirectWrite -> DirectComposition (power-efficient GPU first)
  gfx::stage   the composition window + swap chain (never resized mid-animation)
  gfx::render  display list -> Direct2D          gfx::pill  the collapsed silhouette -> CPU layered window
  services::*  OS-backed event producers (media, clipboard, shelf, notifications, calendar, privacy,
               downloads, stats, control, phone), alive only while their module is
  win::*       sampler, fullscreen watcher, session, tray, hotkeys, autostart, single instance, ...
  diag         the self-test script

notch-preview (dev only)   display list -> PNG with tiny-skia, so layouts can be inspected anywhere
```

**Modules never touch pixels.** They emit a display list (shapes, text, icons, hit regions); the
Direct2D backend and the PNG previewer execute the *same* list. The shell is a pure state machine
driven by an injected clock, so springs, stagger, hover dwell, suppression and fullscreen decisions are
unit-tested on any OS.

**One event bus, a source tag on every event.** A clipboard item, a battery level or a Focus change is
the same small typed event whether it came from this PC or from the iPhone; only `source` differs
(`Local` | `Phone`), so the UI draws both the same way. Transports (the iPhone listener is one) are
ordinary services that produce events; modules never call Win32, they send `Command`s.

**How idle stays ~0 %.** The loop waits in `MsgWaitForMultipleObjectsEx` with an infinite timeout unless
a deadline is pending; the swap chain's frame-latency handle is in the wait set *only while animating*;
a module's poll runs only while its page is expanded and visible; the idle pill is a few-kilobyte
bitmap in a layered window, and the GPU stack exists only while something is drawn (it is pre-warmed
when the cursor approaches, so the first animation does not wait for it).

## Performance

The budget was ~10–30 MB of RAM and ~0 % CPU at idle, with the render loop running only during
animations. Measured by `--selftest` on a CI virtual machine (a **software renderer**, 1024×768,
64 Hz; the full report and the numbers per phase are in [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)):

| | |
|---|---|
| Idle CPU, pill collapsed | **0.04–0.05 %** of one core (exact cycle counts over 5 s, the app alone; no timers, the only recurring wake-up is the 10 Hz cursor sample) |
| Memory, GPU stack warm | **11.6 MiB** private working set at idle (40 MiB working set incl. shared DLLs) |
| Memory, GPU stack released | **4.8 MiB** committed (the working set is about 1.4 MiB) |
| Memory after a session that opened every page | **20 MiB** committed with the GPU released; more with the GPU warm (see [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)) |
| Bringing the GPU stack back | about 40–60 ms; creating it cold: 186 ms (so it is pre-warmed when the cursor approaches) |
| A page open and polling (stats, once a second) | 0.4–0.5 % of a core; **nothing** is read once it closes |
| The iPhone listener, switched on, nobody connected | about 0.05 % (a thread asleep in `accept()`), the same as off |

**Read these with care.** A software renderer is not your GPU: a real driver loads tens of MB of
user-mode DLLs, which is exactly why the GPU stack is released when nothing is drawn, but it also
means *your* warm number will be higher than 11.6 MiB. The same VM also made frames hitch, and the
report does not hide it: of 741 frames in animations, 77 were later than 1.5× the refresh interval,
half of them because the software renderer needs about 16 ms to draw an opening panel, half
because the virtual display presented late; a few frames of 100–800 ms spent all their time inside
Direct2D's `EndDraw` (see [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) for what is and is not known
about them). Nothing here proves your machine hitch-free: run `shark-notch.exe --selftest` on it.

## What is and is not verified

Three tiers, and each claim in the docs says which one it is in:

1. **Tested on any OS** — everything that is logic (springs, state machines, config, the event bus, every
   module's views and behaviour, the calendar and clipboard engines, the iPhone protocol and its
   security checks, fuzzed against garbage) runs under `cargo test` on Linux *and* on Windows.
2. **Run for real on a virtual Windows machine in CI** — the app builds with the real MSVC toolchain,
   and `--selftest` drives the real windows, the D3D11/D2D/DirectComposition stack (software renderer),
   the real clipboard, the real drop target, a loopback client against the real iPhone listener, and so
   on, checking what comes out. No GPU, no audio device, no battery, no radios, no microphone, no
   notification access and no iPhone exist there.
3. **Needs your hardware** — a real GPU driver (the memory numbers CI prints are a software
   renderer's), your monitors and DPI, SMTC with a real player, real drag and drop, the Windows
   notification listener, a laptop's battery and panel, radios, real browsers' downloads, real
   calendar feeds, an iPhone with Shortcuts, the Windows Firewall prompt, and anti-cheat software.
   Each module's document ends with its own list. Where something is **not feasible at all**, the
   document says so instead of pretending (Explorer copy progress, changing Windows' do-not-disturb).

## Roadmap

| # | Phase | State |
|--:|-------|-------|
| 1 | Shell (window, springs, hover/hotkey, tray, fullscreen, config) | implemented — see [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) |
| 2 | Event bus + `Module` trait + clock module | implemented |
| 3 | Media (SMTC, album art, controls, seek, visualizer) | implemented — needs your hardware for the live SMTC check |
| 4 | Clipboard history (text, links, images; pin; re-copy) | implemented |
| 5 | File shelf (OLE drop target, drag out) | implemented — real drag-and-drop needs your hardware |
| 6 | Notifications (banner) | implemented for the iPhone link only, as a pop-up with no page; reading Windows' own notifications was dropped (it needs a package identity and never worked here) |
| 7 | Calendar (ICS feeds, month view, Join banner, countdown chip) + Pomodoro with tasks | implemented — real feeds and typing need your machine, see [`docs/CALENDAR_AND_FOCUS.md`](docs/CALENDAR_AND_FOCUS.md) |
| 8 | Live activities (microphone/camera chip, quick timers, browser downloads with *Show in folder*) | implemented — the privacy chip and real browsers need your machine; copy progress is **not** feasible, see [`docs/LIVE.md`](docs/LIVE.md) |
| 9 | System stats (CPU, memory, GPU, network, battery; read only while the page is open) | implemented — GPU counters and battery need your hardware, see [`docs/STATS.md`](docs/STATS.md) |
| 10 | Command centre (volume, brightness, Wi-Fi, Bluetooth, Focus state, snip) | implemented — every control needs your hardware to be proven; Focus can only be read, see [`docs/CONTROL.md`](docs/CONTROL.md) |
| 11 | iPhone link (a small server on your own network that iOS Shortcuts send to: clipboard text and links, files, battery, Focus) | implemented — **off by default**, plain HTTP, a real iPhone was not available to the build; read [`docs/IPHONE_SHORTCUTS.md`](docs/IPHONE_SHORTCUTS.md) before switching it on |

## Attribution and licence

The UX was *inspired* by [Bloom](https://github.com/SehajveerSingh2005/bloom) (GPL-3.0): which pages
exist and how scrolling cycles them. No Bloom code, assets or layouts are used here; everything was
written from scratch. This repository has no licence file yet; that choice is yours.
