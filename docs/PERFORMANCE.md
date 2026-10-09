# Performance: what was measured, where, and what it is worth

The brief set a budget (about **10–30 MB** of RAM and about **0 % CPU** at idle, the render loop
running only during animations, no dropped frames) and asked for a frame-time report after every
animation. This page says how each of those was measured, what came out, and — as important — what
the numbers cannot tell you.

> **The honest summary.** Every number below comes from a **virtual Windows machine** on GitHub
> Actions with a **software renderer** ("Microsoft Basic Render Driver", 1024×768, 64 Hz), shared
> with other jobs. It has no GPU, so the memory a real graphics driver adds is *not* in these
> figures, and its frame times are a CPU rasteriser's. The idle CPU numbers are meaningful (nothing
> in them depends on a GPU); the memory and frame numbers are indicative. **Run `--selftest` on your
> own machine for the numbers that count.**

## The budget, and where it stands

| Metric | Budget | Measured (CI virtual machine) | Worth |
|--------|--------|-------------------------------|-------|
| Idle CPU, collapsed pill | ≈ 0 % | **0.02-0.04 %** of one core over 5 s, with the GPU stack warm or released (exact cycle counts, the app alone). The only recurring wake-up is the 10 Hz cursor sample | Meaningful: nothing in it depends on a GPU |
| Memory with the GPU stack warm | 10–30 MB | **12.7 MiB** private working set at idle; 41 MiB working set (shared system DLLs included) | Indicative: a real driver adds its user-mode DLLs on top |
| Memory with the GPU stack released | ≈ 5–10 MB | **6.4 MiB** committed; the working set is trimmed to **0.2 MiB** private | Meaningful for commit; the trimmed working set is a snapshot (see below) |
| Cost of bringing the GPU stack back | not on the hover path (it is pre-warmed when the cursor approaches) | **39 ms** from released; **174 ms** the first time (software device) | Indicative |
| A page open and polling (stats, 1 Hz) | low | **0.3 %** of one core; **nothing** measured once the page closes | Meaningful |
| The iPhone listener, on, nobody connected | ≈ 0 % | **0.02 %**, the same as without it: one thread asleep in `accept()` | Meaningful |
| After a session that opened every page and exercised every module | 10–30 MB | **24 MiB** committed with the GPU released; the private working set with the GPU warm late in the script was 26–43 MiB | Over the budget when everything has been used and the GPU is warm; see [Memory after a full session](#memory-after-a-full-session) |

Not measured at all, because nothing there could: a real GPU's memory, the cost of the first frame
on a real driver, a 144 Hz display, an HDR monitor, per-monitor DPI changes, a laptop on battery.

## How to measure on your machine

```powershell
cargo build --release -p shark-notch
.\target\release\shark-notch.exe --selftest --out selftest.txt
```

It runs the real app (real windows, the real GPU stack, the real message loop) through a scripted
2-minute session, prints everything below, and exits with 0 if every check held and 1 otherwise. It
uses a scratch data folder and its own default settings: your configuration and saved data are
untouched. Optional switches:

| Switch | Adds |
|--------|------|
| `--no-exclude --light-probe` | The notch is normally hidden from screen capture and the self-test's screen read would see nothing; these two let it read the *composed desktop* pixels and check that the pill and the panel are really there (and that album art and a clipboard thumbnail reach the screen) |
| `--registry-probe` | Writes (and removes) a fake "microphone in use" record in your registry's consent store, to prove the privacy chip appears and goes. Only on request, never by default |

The tray's *Copy frame-time report* and *Copy
diagnostics*.

### Reading it

```
SELFTEST gpu stack warm on '<adapter>'; created in 173.9 ms          the first creation of the GPU stack
SELFTEST idle (GPU warm) over 5.0s: cpu 0.0210% (whole process incl. the self-test's own heartbeat thread: 0.0769%; ...)  private WS 12.7 MiB  working set 41.0 MiB  commit 20.8 MiB
SELFTEST idle (GPU released) over 5.0s: cpu 0.0256% (...)  private WS 0.2 MiB  working set 0.7 MiB  commit 6.4 MiB
SELFTEST warm from released state took 38.8 ms
...one line per scenario, each ending in what was checked...
frame-time bursts (hitch = interval > 1.5x refresh period):
burst 'expand': 12 frames over 198 ms (refresh 15.62 ms) interval avg 17.97 p50 15.82 p95 37.78 p99 37.78 max 37.78 ms | cpu avg 14.42 worst 32.45 ms | HITCH x4 (slow drawing 4, late presentation 0)
frames presented: 973  errors: 0
whole run: 40 animation bursts, 782 frames; 89 hitches (frames over 1.5x the refresh interval): 44 where the frame itself was slow to draw, 45 late for another reason (presentation); longest interval 818.5 ms; ...
memory: private working set 12.7 MiB (GPU warm) -> 0.2 MiB (GPU released); idle CPU of the app 0.0210% / 0.0256%
SELFTEST no system stalls: a helper thread was never delayed by more than 120 ms
SELFTEST result: PASS
```

* **cpu** is the share of one core the process used, from exact cycle counts (`QueryProcessCycleTime`)
  divided by a calibrated cycles-per-second figure. The "scheduler ticks" number beside it is
  Windows' own accounting, which has 15 ms granularity and is only here to show the two agree in
  order of magnitude. The self-test runs a helper thread of its own (the heartbeat, below); the
  headline figure leaves its cycles out and the whole-process figure beside it includes them.
* **private WS** is the private part of the working set: what Task Manager's *Memory* column shows.
  **working set** adds shared pages (system DLLs). **commit** is private bytes the process has
  allocated, resident or not.
* **Why the released working set is tiny.** Releasing the GPU stack calls `EmptyWorkingSet`, so the
  working set is trimmed to almost nothing and pages come back as they are touched. That makes the
  *snapshot* small; the honest footprint of the released state is the **commit** (6.4 MiB here).
* **A burst** is the run of frames of one animation (expand, collapse, peek, page switch). `interval`
  is the time between presented frames, **`cpu`** is the time the app spent building and drawing a
  frame (update + compose + Direct2D + present call). A **HITCH** is a frame whose interval was more
  than 1.5× the refresh period. It is **slow drawing** when the frame itself took longer than one
  refresh period to build, and **late presentation** when it was built quickly but arrived late (the
  display's or the machine's pacing, not the app's work).
* **A slow-frame line** (a frame of more than 2.2 refresh periods) says where its time went: getting
  the target ready, issuing the display list, Direct2D's `EndDraw`, and how much text-layout, geometry
  and image-upload building the frame had to do. Those are the places the *app* can spend time in.
* **The heartbeat** is a helper thread that sleeps 15 ms at a time and notes every time it woke
  more than 120 ms late. If it reports a stall, the whole *process* was not scheduled (a busy or
  paused VM), and a hitch at that moment is not the app's.
* **Looks at the render thread.** When a frame sits inside `EndDraw` for more than 40 ms, the
  heartbeat stops the render thread for a few microseconds, copies its instruction pointer and the top
  of its stack, starts it again, and prints which modules the addresses belong to (a system call's
  name, or "generated code" for a software renderer's compiled shaders), and how much of the elapsed
  time the thread itself was executing. Nothing is injected anywhere; it only runs under `--selftest`.

CI keeps the report as the `selftest.txt` artifact of every run and prints it in the job log.

## The frame-time report: what it says, and what it does not

Run 25 (the last one measured): **951 frames presented, 0 errors**; 763 of them in 41 animation bursts, and
**79 of those were hitches** (interval over 1.5x the refresh period, 15.6 ms at 64 Hz): 41 *late
presentation* (built in under a millisecond, arrived a period late: the virtual display's pacing) and
38 *slow drawing*. The heartbeat saw no stall of the machine. Mean CPU per frame on the software
renderer: expand 15 ms, banner 13 ms, page switch 13 ms, collapse 4 ms (0.2 ms typical). A GPU does
not pay these.

**The long frames (100-522 ms; 13 in run 25).** For every frame of more than 2.2 periods the log
says where the time went. In all of them set-up, the display list, text layouts, geometry and image
uploads cost about 0 ms; the time is inside Direct2D's `EndDraw`. A watcher thread (self-test only)
looked at the render thread during 45 such waits: it stood in
`win32u!NtGdiDdDDIWaitForSynchronizationObjectFromCpu`, called from `d3d11`/`d3d10warp` (the software
device), and executed 0.6 % of the time (24 ms of 4142 ms). While it waited, all threads of the process
together used 1-10 ms of CPU in about 80 ms. So the app was **waiting on the graphics device or
compositor of the virtual machine**, not working. Twice a wait was a first-use DLL load inside
Direct2D (`LoadLibraryExW`).

Not the cause, each tried: clearing Direct2D's caches after every animation (hitches 88 -> 88), a
32 MiB instead of 8 MiB texture budget (86-97 either way), building text/geometry/images (0 ms).
Pattern found late: these frames are **not** the first of their burst (they were frames 7-29), and
across 150 bursts in four runs, 48 of the 50 bursts with a 100 ms+ frame were bursts whose frames
had been presented far faster than the refresh rate (median interval under 3 ms); only 2 of 63 paced
bursts had one. That points at presents queuing faster than this virtual display consumes them. A
limiter on the present rate is the obvious next experiment; **it has not been run**. Nothing here
shows your machine is free of such frames: run `--selftest` and look at the "render thread" lines.

## Memory after a full session

With the GPU released the commit is **4.9 MiB** (the working set 1.5 MiB). After a script that opens
every page and exercises every module it is **20.3 MiB** (20.6 and 27.3 MiB in the neighbouring runs),
of which the default heap holds 6.1 MiB in use of 8.7 MiB committed; the working set peaked at 53-64 MiB
while the software GPU stack was warm. Just after each release the heap still showed 9-44 MiB committed
and gave it back over the following seconds: the software renderer's own allocations live in the
process heap, which is mostly why the late-session working set with the GPU warm reads 28-43 MiB.
Opting into the Windows *segment heap* (manifest) lowered the end-of-session commit from 27.3 to about
20.5 MiB and the heap's committed size from 34.9 to 8.7 MiB.

So the 10-30 MB budget holds for the state the notch is in nearly all the time (collapsed, GPU
released) and for a fresh session with the GPU warm (about 12 MiB, software renderer). It does **not**
hold, on this evidence, for a session that has opened every page *and* has the GPU stack warm
(28-43 MiB here). The levers: `[modules]` (a module that is off costs nothing) and
`gpu_idle_release_secs` (default 300).

## By phase

One row per phase: the self-test of the phase's own CI run, or of the first later run whose
self-test passed when the phase's own run failed a functional check (those runs failed for reasons
that had nothing to do with performance: a unit test, the event-bus wake-up bug that was fixed along
the way, a check that was wrong, a lint). All of them ran on the same kind of virtual machine and
the same software renderer.

| Phase | Commit | Idle CPU warm / released ¹ | Private WS idle, warm / released | GPU stack: first creation / re-creation | Frames in the run | Hitches in the run ² | Longest frame |
|-------|--------|---------------------------|----------------------------------|----------------------------------------|------------------:|---------------------:|--------------:|
| 1 Shell | `cbba3fb` | 0.000 / 0.000 % ³ | 8.5 / 0.2 MiB | 26 / 17 ms | 103 | 9 | 64 ms |
| 2 Event bus, modules, clock | `f2684fc` | 0.046 / 0.046 % | 6.4 / 0.3 MiB | 52 / 36 ms | 72 | 4 | 64 ms |
| 3 Media | `9ca406b` | 0.035 / 0.035 % | 6.8 / 0.2 MiB | 115 / 25 ms | 113 | 15 | 1370 ms ⁴ |
| 4 Clipboard | `c619fad` | 0.235 / 0.255 % | 9.4 / 0.2 MiB | 237 / 31 ms | 193 | 28 | 251 ms |
| 5–6 Shelf, notifications | `293c9c2` | 0.154 / 0.146 % | 10.8 / 0.2 MiB | 800 ⁵ / 32 ms | 369 | 45 | 443 ms |
| 7 Calendar, Pomodoro | `e8fb0d4` | 0.331 / 0.328 % | 11.3 / 0.2 MiB | 223 / 49 ms | 481 | 85 | 423 ms |
| 8–10 Live, stats, controls | `3ef2a8b` | 0.133 / 0.122 % | 11.7 / 0.2 MiB | 167 / 48 ms | 852 | 91 | 413 ms |
| 11 iPhone link | `4d7bb8c` | 0.171 / 0.163 % | 12.3 / 0.2 MiB | 134 / 64 ms | 946 | 88 | 681 ms |
| after 11: diagnostics | `58c1914` | 0.021 / 0.026 % ¹ | 12.7 / 0.2 MiB | 174 / 39 ms | 973 | 89 | 818 ms |

¹ **From phase 4 on the idle CPU figure includes the self-test's own heartbeat thread**, a helper
that wakes 66 times a second to notice stalls of the whole machine. It first existed in phase 4,
which is where the figure jumps from about 0.04 % to about 0.2 %; phases 2 and 3 (no heartbeat) are
the best evidence of what the app itself costs at idle. From commit `3a036a5` the report leaves the
thread out and prints both numbers: **the app alone is 0.02–0.04 %** (the last row; the whole process,
heartbeat included, was 0.08 % in that run).

² Whole-run sums. Until `3a036a5` the report printed "total hitches" for the last 16 bursts only,
while "frames presented" counted the whole run, so the printed totals under-counted from phase 5
on; the column is the sum of every burst's own warning in the log.

³ Phase 1 used scheduler ticks only (below one 15.6 ms tick in 5 s); exact cycle counting starts in
phase 2.

⁴ Phase 3's worst is a two-frame 'peek' burst whose frames were 1.4 s apart with 2.7 ms of CPU: not
CPU-bound. The heartbeat did not exist yet, so a stall of the virtual machine cannot be excluded.

⁵ One outlier on a cold machine: 694 of the 800 ms was the pre-render of every page, against
116–136 ms in runs on almost the same code.

What the history shows:

* **Idle memory grew by a few MiB over ten modules and the released footprint did not.** Private
  working set at idle with the GPU warm: 6.4 MiB (phase 2) to 12.3 MiB (phase 11), +2.6 at the
  clipboard, +1.4 at the shelf and notifications, about +0.5 per later step; committed bytes with the
  GPU released 4.6 → 6.5 MiB. The released working set is the trimmed snapshot (0.2–0.3 MiB).
* **The slow frames are all inside Direct2D drawing.** Across 13 self-test runs, all 192 "slow frame"
  warnings had update + compose ≤ 0.3 ms and present ≤ 0.8 ms; render was at least 95 % of the frame.
  The worst ones are the first draw of something new (the first notification banner cost 379–483 ms
  in 7 of the 9 runs that have it; the Live timer banner 413 and 680 ms).
* **Some "hitches" are not drawing at all.** In the phase 8–10 run, 35 of the 91 sat in bursts whose
  worst interval was ≤ 35 ms (one missed 15.6 ms tick, mostly collapses with under 2 ms of CPU): the
  virtual display's pacing, not load.
* **The heartbeat never reported a stall of the machine** in the ten runs that have it; in the two
  runs that failed a timing check, the notch's own loop was the busy one.
* **Costs of a page and of the listener:** the stats page open costs 0.3–0.5 % of a core and nothing once
  closed; the iPhone listener switched on with nobody connected costs the same as having it off.


## What each phase added, and what it cost

* **Idle stays flat as modules are added** because a module that is switched off is never created
  and a module that is on does nothing until it is on screen: a page's poll runs only while its
  page is expanded and visible (stats once a second, the command centre once a second, the iPhone
  page every 5 s), the workers sleep on events (the clipboard listener, the notification listener,
  `ReadDirectoryChangesW`, the registry change notification for the microphone chip, `accept()`),
  and the stats and command-centre workers do not exist until their page first asks.
* **The only recurring wake-up** is the 10 Hz cursor sample, which stops completely while paused,
  locked, display-off, or when a fullscreen app is in front. (Windows has no event for "the cursor
  entered this region" that works without hooks, which the brief ruled out; see `DESIGN.md` D3.)
* **The GPU stack exists only while something is drawn.** The collapsed pill is a few-hundred-byte
  bitmap in a layered window; the D3D/D2D/DirectComposition stack is created when the cursor
  approaches (or a hotkey, a banner or a chip needs it) and released after `gpu_idle_release_secs`
  (300 s) of nothing to draw. A chip drawn on the pill (a timer, the next meeting, a microphone in
  use) keeps it resident while it shows, because the chip is animated text; the report's "GPU kept
  for it" lines check that it is released again afterwards.
* **Nothing is measured while nobody looks:** the self-test counts readings after each page closes
  (stats, command centre) and fails if any arrive.

## Caveats worth repeating

* The numbers come from one VM image on one day. They vary run to run, and the table's trend matters
  more than any single digit.
* "Private working set" is the figure the brief's "10–30 MB" is most naturally about, and it is the
  one a real GPU driver inflates (user-mode driver DLLs and shader caches are tens of MB). If your
  warm number is above the budget, the knob is `gpu_idle_release_secs` (shorter) — or `0` to keep the
  GPU stack always warm if you would rather have the first hover instant than the memory.
* Nothing here says anything about **battery** impact on a laptop. The design aims at a pill with no
  timers and a 10 Hz sample; measuring it needs a power meter or Windows' energy estimation on
  your machine.
