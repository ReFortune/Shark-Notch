# Shark Notch

A tiny, animated overlay at the top-centre of your Windows screen — the MacBook notch / Dynamic Island
idea — written in **Rust** with the `windows` crate. No Electron, no WebView2, no browser engine:
**Direct2D + DirectComposition + DirectWrite**, one small process, event-driven everywhere it can be.

> **Status.** Built phase by phase; see [`docs/DESIGN.md`](docs/DESIGN.md) for the design and the
> decisions you may want to veto, and [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) for what was measured,
> where, and what still needs *your* hardware. Phases implemented so far are listed under
> [Roadmap](#roadmap).

## What it does

* **Shell** — a borderless, always-on-top, click-through pill hugging the top edge of your monitor.
  Hover the top-centre for ~150 ms (or press <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>N</kbd>) and it
  springs open; scroll or swipe to switch pages; move away and it springs shut.
* **Premium motion** — width, height, corner radius, ear size and position are *independent springs*;
  every transition is interruptible and keeps its velocity; the shape moves first and the content
  follows; vsync-locked and refresh-rate aware; respects Windows' "Animation effects" setting.
* **Stays out of the way** — never takes focus, no taskbar button, hides from screen shares by
  default, steps aside (and frees the GPU) while a game or fullscreen app is in front, uses no
  hooks and no injection — `RegisterHotKey` is the only global input mechanism.
* **Light** — the idle pill is a few-hundred-byte bitmap in a layered window; the GPU stack exists
  only while it is being used (and is pre-warmed when the cursor approaches).

## Build and run

You need Windows 10 (2004+) or 11, the Rust stable toolchain (edition 2024, `rustup`), and the MSVC
build tools.

```powershell
cargo build --release -p shark-notch
.\target\release\shark-notch.exe          # runs in the tray; Ctrl+Alt+N toggles the notch
```

On first run a commented `config.toml` is written to `%APPDATA%\SharkNotch\` (tray menu →
*Open settings*). Saved changes apply immediately; a mistake keeps the previous settings and shows
the error in the tray tooltip. Logs: `%LOCALAPPDATA%\SharkNotch\notch.log`.

Useful command-line switches:

| Switch | Effect |
|--------|--------|
| `--selftest [--out file]` | Scripted run of the real app; prints frame-time bursts, idle CPU/RAM with the GPU warm and released, and the GPU warm-up cost. Add `--no-exclude --light-probe` to also probe the actual screen pixels. |
| `--config path` | Use a different config file. |
| `--console` | Echo the log to the console that launched it. |

### On other operating systems

The platform-independent core builds and tests anywhere:

```bash
cargo test -p notch-core                 # 150+ tests: springs, shell, hover, config, bus, module host...
cargo run -p notch-preview -- shell out/shell.png     # render the shell's choreography to a PNG
cargo run -p notch-preview -- modules out/modules.png # ...or the real module host (chips, pages, peek)
cargo check --target x86_64-pc-windows-msvc -p shark-notch   # type-check the Windows app
```

## Architecture (one page)

```
notch-core  (portable, #![forbid(unsafe_code)], all logic + tests)
  spring · path (continuous corners) · shell (state machine) · hover (FSM) · compose
  config · hotkey · input · fullscreen (decision) · frame (pacing) · raster · icons · draw (display list)

shark-notch (Windows glue only)
  app          the controller + single message loop; GPU residency; suspension; hot reload
  gfx::stack   D3D11 -> DXGI -> D2D -> DirectWrite -> DirectComposition (power-efficient GPU first)
  gfx::stage   the composition window + swap chain (never resized mid-animation)
  gfx::render  display list -> Direct2D          gfx::pill  display list -> CPU layered window
  win::*       sampler, fullscreen watcher, session, tray, hotkeys, autostart, single instance, ...

notch-preview (dev only)   display list -> PNG with tiny-skia, so layouts can be inspected anywhere
```

The key idea: **modules never touch pixels**. They emit a display list (shapes, text, icons, hit
regions); the Direct2D backend and the PNG previewer execute the *same* list. The shell is a pure
state machine driven by an injected clock, so springs, stagger, hover dwell, suppression and
fullscreen decisions are unit-tested on any OS.

How idle stays ~0 %: the loop waits in `MsgWaitForMultipleObjectsEx` with an infinite timeout unless a
deadline is pending; the swap chain's frame-latency handle is added to the wait set *only while
animating*; the only recurring wake-up is a 10 Hz cursor sample that stops entirely while paused,
locked, display-off or a fullscreen app is foreground.

## Roadmap

| # | Phase | State |
|--:|-------|-------|
| 1 | Shell (window, springs, hover/hotkey, tray, fullscreen, config) | implemented — see `docs/PERFORMANCE.md` |
| 2 | Event bus + `Module` trait + clock module | implemented |
| 3 | Media (SMTC, album art, controls, seek, visualizer) | implemented — needs your hardware for the live SMTC check |
| 4 | Clipboard history (text, links, images; pin; re-copy) | implemented |
| 5 | File shelf (OLE drop target, drag out) | implemented — real drag-and-drop needs your hardware |
| 6 | Windows notifications (banner, list, missed badge) | implemented — needs a one-off identity step and your hardware, see [`docs/NOTIFICATIONS.md`](docs/NOTIFICATIONS.md) |
| 7 | Calendar (ICS feeds, month view, Join banner, countdown chip) + Pomodoro with tasks | implemented — real feeds and typing need your machine, see [`docs/CALENDAR_AND_FOCUS.md`](docs/CALENDAR_AND_FOCUS.md) |
| 8 | Live activities | planned |
| 9 | System stats | planned |
| 10 | Command centre | planned |
| 11 | iPhone listener | planned |

## Attribution and licence

The UX was *inspired* by [Bloom](https://github.com/SehajveerSingh2005/bloom) (GPL-3.0): which pages
exist and how scrolling cycles them. No Bloom code, assets or layouts are used here. This repository
has no licence file yet; that choice is yours.
