# Windows notifications in the notch

The notifications page mirrors your Windows toasts: a new one shows as a banner for a few seconds
and then tucks away; the page lists the latest ones. **Reading other apps' notifications needs a
step from you** — this document says what, why, what it changes on your machine, and which parts
nobody has been able to test yet.

## What works without any setup

* Everything else in the notch.
* **iPhone notifications** (phase 11) are delivered by Shark Notch's own listener and need no Windows
  permission at all. They use the same banner and the same page.
* The page tells you plainly when Windows notifications are unavailable ("Windows notifications
  need app identity"), instead of staying empty.

## Why a plain `shark-notch.exe` cannot read them

Windows exposes other apps' notifications through `UserNotificationListener`, and it only answers a
process that has **package identity** — the identity an app gets from being an MSIX/Store package.
A normal downloaded `.exe` has none, and there is no setting, manifest flag or registry key that
changes that for an unpackaged process. (The alternatives people sometimes reach for — scraping the
notification centre through UI Automation, or reading `wpndatabase.db` — are fragile, can break with
any Windows update, and for the database route can read content you never meant to share. Shark
Notch does neither.)

## The way around it: a *sparse package*

A sparse package is a manifest-only MSIX package that **points at your existing exe**. The exe is not
repackaged, copied, moved or modified (apart from the manifest that is already embedded at build
time, `packaging/shark-notch.exe.manifest`, which does nothing until a package is registered).
Registering the package makes Windows give the running exe an identity, and the identity declares the
`userNotificationListener` capability.

Files (all in `packaging/`):

| File | What it is |
|------|------------|
| `AppxManifest.xml` | the sparse package manifest (identity `SharkNotch`, capability `userNotificationListener`, points at `shark-notch.exe`) |
| `Assets/` | the three logos the manifest requires |
| `register-identity.ps1` | registers the package |
| `unregister-identity.ps1` | removes the package and the certificate again |
| `shark-notch.exe.manifest` | embedded in the exe at build; its `<msix>` element links the exe to the package |

### Option A — signed package (default; works without Developer Mode)

Needs the **Windows SDK** ("Signing Tools for Desktop Apps" and "MSIX Packaging Tools" provide
`signtool.exe` and `makeappx.exe`) and an **elevated Windows PowerShell 5.1** (run as administrator,
*from your own account*: the package is registered for the user who runs it).

```powershell
cd path\to\Shark-Notch\packaging
.\register-identity.ps1 -ExePath C:\path\to\shark-notch.exe
```

What it does, in order — so you can judge it before running it:

1. creates a self-signed *code-signing* certificate `CN=SharkNotch` in **your** certificate store with
   a **non-exportable** key;
2. builds `SharkNotch.msix` from the manifest and logos (`makeappx pack /nv`) and signs it;
3. adds the certificate's **public** part to the machine's **Trusted People** store (this is what
   lets Windows install a package signed by it; it is the step that needs administrator rights);
4. installs the package for you with the exe's folder as external location;
5. **deletes the private key** again, so nothing can ever be signed with that certificate afterwards.

Security implication, plainly: while the public certificate is in *Trusted People*, Windows would
accept packages signed with that certificate on this machine. Because the key is non-exportable and
deleted in step 5, no one can sign anything with it, but the entry stays until you run
`unregister-identity.ps1`. If you would rather not trust any certificate, use option B.

### Option B — Developer Mode (no certificate, no SDK)

Turn on *Settings → System → For developers → Developer Mode* (this is a system-wide setting with
its own implications: it lets you sideload apps from anywhere), then:

```powershell
.\register-identity.ps1 -ExePath C:\path\to\shark-notch.exe -DeveloperMode
```

This registers the unsigned manifest directly (kept in `%LOCALAPPDATA%\SharkNotch\identity`).

### Afterwards

1. Quit Shark Notch from its tray icon and start it again (from the same location you registered).
2. The first time, Windows may ask whether to let the app access your notifications; you can also
   switch it under *Settings → Privacy & security → Notifications*. If you chose "no", the page says
   so and offers *Open settings*; opening the page after changing the setting looks again.
3. The page now lists your notifications. Test with any app that posts a toast.

To undo everything: `.\unregister-identity.ps1` (certificate removal needs the elevated shell).
Your exe is not touched by either script.

## What the notch does with your notifications

* The text (title and a few body lines, each cut to a sane length) and the app's name and logo go
  from Windows to the notch's window **in memory** and nowhere else. **Nothing is written to disk and
  nothing is logged** — the log contains counts and ids only.
* Notifications that were already waiting when the notch started are listed but never announced.
* **Do-not-disturb / Focus** is honoured: while Windows is in quiet time the banner is not shown
  (the entry is still listed).
* **A fullscreen game is never interrupted.** While the notch is suspended (fullscreen app, pause,
  lock) notifications are kept silently; afterwards the pill grows a small bell badge with a count and one "While you were away: N notifications" banner shows; opening the page clears the
  badge. (Setting `fullscreen.show_missed_indicator = false` turns the badge and the summary off.)
* Dismissing in the notch hides *the notch's copy*. Whether it also clears the notification from
  Windows' notification centre is `notifications.dismiss_in_windows` (default `false`: the notch
  never deletes anything you did not ask it to).
* If you share your screen, the notch's window can be hidden from capture
  (`general.exclude_from_capture`, on by default); a notification banner contains the same text a
  Windows toast would.

Configuration (`config.toml`):

```toml
[notifications]
enabled = true
peek = true               # show a banner for new notifications
peek_secs = 4.0
max_items = 20
ignore_apps = []          # e.g. ["Spotify"]
dismiss_in_windows = false
```

## How it works (and what costs what)

One worker thread exists only while the module is enabled. With identity it asks for access, subscribes
to `NotificationChanged` and sleeps; there is no polling. On each change it reads the *whole* list once
and compares it with what it announced before (the event says that something changed, not reliably
what), then sends `Notification` / `NotificationRemoved` events on the bus. App logos are decoded once
per app (at most 64 apps, 64×64 px each). Without identity the thread sleeps forever after one status
event.

## Honest status — what has and has not been verified

Verified in automated tests (portable core + Windows CI): the bookkeeping that compares notification
lists, the text clean-up and length limits, the banner / list / badge / dismiss behaviour, the
"missed while a game was in front" flow end-to-end through the real suspension path (windows hidden,
GPU released, badge, summary banner, GPU kept only while the badge shows), the *no identity* path
(the CI runner is a plain exe, so it is exactly what an unregistered install looks like), and that the
exe really embeds its manifest.

**Not verified — nobody has run these on a machine with identity and real notifications:**

* the sparse-package registration itself (`register-identity.ps1`, `AppxManifest.xml`): written from
  Microsoft's documentation, **never executed**; the scripts have not even been parse-checked by
  PowerShell in the environment this was written in. Expect that something small may need adjusting;
  the error messages are meant to say what;
* the access prompt and what `RequestAccessAsync` does from a background thread of a packaged
  desktop app;
* reading real toasts: text extraction from the generic binding, app logo retrieval for classic
  Win32 apps, and the reliability of `NotificationChanged` (Windows has been known to drop or delay
  it; the notch re-reads the list each time, and so also when the page is opened without access,
  but there is no timer that would catch a lost event).

If you try it and something is off, `shark-notch.exe --selftest` and the log
(`%LOCALAPPDATA%\SharkNotch\notch.log`) show what the service reported.
