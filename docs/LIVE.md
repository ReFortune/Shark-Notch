# Live activities (privacy indicator, timers, downloads)

One page, "Live", with three things that are happening *right now*. `privacy` and `downloads` have
their own switches in `[live]` of `config.toml` (`enabled = false` turns the whole module off, and
removing `"live"` from `[modules] order` hides the page).

The collapsed pill has room for the most important activity only: **privacy first** (a microphone you
did not expect to be live must never hide behind a timer), then a **timer**, then a **download**. The
page lists everything. Nothing here polls on a timer: the platform pushes events, and the only
wake-ups are a timer's end, the minute turning over on a timer chip, and — while a download is being
written — a size check once a second.

## Microphone and camera in use

A chip with the program's name appears while a program is using the microphone or the camera, and
the page lists them ("Zoom — Using the microphone").

**How it knows.** Windows keeps a usage record for every program and device class — the same records
behind *Settings → Privacy & security → Microphone / Camera → "Recent activity"* — under
`HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{microphone,webcam}`:
`LastUsedTimeStart`, and `LastUsedTimeStop`, which is **0 while the device is in use**. The app waits
on a registry change notification (`RegNotifyChangeKeyValue`) and re-reads the records when they
change. No hook, no driver, no polling, no injection, and it never looks at audio or video: it only
reads *which program* and *when*. Program names are shown on screen and kept in memory; they are
not logged.

**What to know**

* It shows what **Windows** records. A program that reaches the device in a way Windows does not
  record (a kernel driver, some exclusive-mode capture tools) is not shown. This is a convenience
  indicator, not a security control; Windows' own tray microphone icon stays the authority.
* A program that dies while recording can leave its record open. Two guards: a record that began
  *before the machine started* is never shown (a power cut), and one that has been "in use" for more
  than 24 hours is ignored. Between those, a crashed program can show for a while until you use it
  again or restart. If this bothers you: `privacy = false`.
* Packaged apps are named from their package (`Microsoft.WindowsCamera_…` → *Windows Camera*);
  desktop programs from their file name (`…\Zoom.exe` → *Zoom*).
* While a fullscreen app is in front the watcher is stopped, and starts again — with a fresh read —
  when it leaves.
* Reading HKCU needs no elevation and no capability. Nothing is written.

## Timers

Quick countdowns: the page has six buttons (`timer_presets`, minutes, default 1 5 10 15 30 60);
tap one to start a timer, **×** to cancel it. At most three run at once, from 1 minute to 10 hours.

* They run on the **wall clock** (a timer stores the second it ends at), so they survive restarting
  the app, the PC sleeping, and the clock being busy. One that ended while the app was closed is
  dropped without a banner.
* While one runs the pill shows a chip with the minutes left (`12m`, turning over once a minute, `1m`
  to the very end). When it ends: a banner ("Timer finished"), and the system chime if `sound = true`
  (never over a fullscreen app; if one was in front, the banner appears when you come back).
* These are separate from the Pomodoro, which has its own page (see `docs/CALENDAR_AND_FOCUS.md`).
* Saved in `%LOCALAPPDATA%\SharkNotch\timers.json` (written a moment after a change, atomically).

## Downloads

A chip with the size so far and the speed (`12 MB/s`) while a browser is downloading, and a banner
**"Download complete — name — size"** with a **Show** button when it finishes. The page lists the
downloads in progress.

**How it knows.** Browsers write a download into a *partial file* next to its final place
(`name.zip.crdownload` in Chrome, Edge, Brave and other Chromium browsers, `.part` in Firefox,
`.opdownload` in Opera, plus `.partial` and `.download`) and rename it to the real name when it is
done. The app asks Windows to tell it when anything in the Downloads folder changes
(`ReadDirectoryChangesW`) and reads only **names and sizes**:

* No percentage and no time remaining — **the file system does not know how big the finished file will
  be**; only the browser does, and the app deliberately does not talk to browsers. You get "so much,
  so fast".
* A partial file is looked at once a second while it exists (NTFS reports size changes lazily); with
  none, the watcher is asleep with no timer at all.
* A partial file that has not grown for 10 minutes stops being listed (a browser that crashed leaves
  them behind); at start-up only partial files written to in the last two minutes count.
* **Show** opens Explorer on the folder with the file selected. That is the *only* action: the app
  **never opens, runs or previews a downloaded file**, so a hostile download is not touched by this
  feature. Only plain absolute paths are ever passed to Explorer.
* File names come from disk and can be hostile, so they are cleaned before they are drawn: control
  and text-direction characters (the "right-to-left override" trick that makes `invoice‮fdp.exe`
  look like a PDF) are removed and long names keep their end, where the extension is.
* The watcher keeps running while a fullscreen app is in front (it costs nothing while idle), so a
  download that finishes during a game gets its banner when you come back.
* `download_dir = ""` watches your Downloads folder wherever Windows says it is; set a folder
  (absolute path) to watch another. Downloads made by programs that write straight to the final name
  (download managers, `curl`, game launchers, torrent clients that use other suffixes) are not seen.

## What is deliberately *not* here: copy / move progress

The brief asked for "download/copy progress where feasible". **Download progress is feasible, as
above; Explorer copy progress is not**, without something this project refuses to do:

* Explorer's progress dialog belongs to Explorer; no API exposes another process's file operations.
* Watching the folders involved only tells you *that* files change, not how big the job is or how far it is.
* Getting the real numbers means injecting into Explorer, hooking its calls, a file-system filter
  driver, or ETW file tracing (which needs administrator rights). Injection and hooks are exactly what
  this app promises never to do (and what anti-cheat software flags), and the others are heavy.

The honest alternative, if you want it: copies **the app itself performs** — for example copying the
shelf's files out to a folder — are under its control and could show real progress. That is not built.

## The self-test

`shark-notch.exe --selftest` exercises this page end to end (timer started from the page and
cancelled; a timer that ends by itself → banner; a partial file growing and being renamed in a
scratch folder → chip, then banner, then **Show** → the reveal request is *recorded*, not executed).

The microphone chip needs a record in the real consent store, so that scenario only runs with
**`--registry-probe`**: it writes a fake record for a program named `selftest-fake.exe` under
`ConsentStore\microphone\NonPackaged`, checks that the chip appears and goes, and deletes the key
again (it is also removed first, in case an earlier run was interrupted). Without the flag nothing
is written to your registry. CI passes the flag.
