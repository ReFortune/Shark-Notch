# Command centre

One page, "Controls": **volume** (with mute), **brightness**, **Wi-Fi** and **Bluetooth** switches, a
**Snip** button and the state of Windows' **Focus** (do not disturb). Switched on or off in
`[control]` of `config.toml`; remove `"control"` from `[modules] order` to hide the page.

```toml
[control]
enabled = true
interval_secs = 1.0   # between readings while the page is on screen (0.5 to 5)
```

It is a page of *controls*, so the honest part first: **nothing here could be verified on real
hardware by the build machine** (a CI virtual machine has no audio device, no laptop panel and no
radios; it did verify that the page and the worker behave properly when all of those are missing).
Each control below says what it uses and where it will not work.

## When anything runs

* **The page asks for a reading only while it is on screen** (the host's poll, once a second by
  default) — so a volume key pressed elsewhere shows up while the page is open, and nothing is read
  while it is not. Dragging a slider pauses the readings so your finger is the source of truth.
* **The worker behind the page does not exist until the page first asks for something.** A command
  centre nobody opens costs nothing: no thread, no COM, no WinRT. After half a minute without
  requests it lets go of everything it opened (the thread then sleeps with no timer).
* The page forgets everything when it goes away, so reopening shows fresh values.
* A slider follows the pointer at once; the level is sent at most 20 times a second while dragging
  and exactly once more on release, and a reading that was already in flight cannot snap the slider
  back for a moment after you let go. A toggle flips at once and holds for a moment while Windows
  catches up.

## The controls

| Control | Uses | Where it will not work |
|---------|------|------------------------|
| **Volume**, **mute** | `IAudioEndpointVolume` on the **default output device** (looked up again for every reading, so plugging in headphones is noticed). The speaker button mutes. | No output device: the row says "No audio output device". This is the master volume only, not per-app volume. |
| **Brightness** | The **built-in panel's** `\\.\LCD` device and the `IOCTL_VIDEO_*_BRIGHTNESS` requests; the driver's list of supported levels is read first and your level is snapped to the nearest one. | Laptops whose driver does not expose brightness this way, and **external monitors** (those need DDC/CI, which is not implemented): the row says "Brightness: not available on this display". It sets the level for both mains and battery, as most brightness tools do. |
| **Wi-Fi**, **Bluetooth** | `Windows.Devices.Radios`. Access is requested when the page first reads the radios, and asked again at every reading while it is not allowed (and after the worker's idle release), so granting it in Settings takes effect without a restart. | No such adapter: "No adapter". Windows' *Let apps control your radios* privacy setting (or a policy) refusing: "Not allowed", and a click opens that setting. A radio switched off by a hardware switch, airplane mode or Device Manager: "Disabled", and a click opens Windows' airplane-mode settings. Switching Wi-Fi off cuts the connection you may be using — including a remote session. |
| **Focus** | `SHQueryUserNotificationState`: **reads** whether Windows is in quiet time. The tile **opens Windows' Focus settings** (`ms-settings:quietmomentshome`). | See below: it cannot *change* Focus. |
| **Snip** | Opens Windows' screen-snip overlay (`ms-screenclip:`: Snipping Tool, or Snip & Sketch on older Windows 10). | The notch captures nothing itself; the snip tool does everything and shows what it captured in its own way. |

## Focus / do not disturb: read-only, on purpose

Windows has **no supported way** for another program to turn do-not-disturb on or off. The ways
that exist are undocumented: writing private WNF state, or editing a binary blob in the registry's
cloud store. They differ between Windows builds, break without notice, and cannot be tested on the
build machine, so this app does not use them. What it does instead is the part that *is* supported:
show the state, and one click away from the page where you change it. If you would like a switch
that always works, the alternative that needs no Windows cooperation is a "quiet banners" switch for
the notch's *own* pop-ups; say the word and it is a small addition.

## Not here

Airplane mode, mobile hotspot, night light, casting and the other Windows quick settings: most have
no supported API, and none was in the brief. Per-app volume and sound-device switching likewise.

## What was verified

* On a Windows machine in CI: the worker answers a request with one reading and sends nothing unasked;
  stopping is prompt; with no audio device, panel or radios the reading is well formed and the page
  says so; the page asks for nothing after it is closed; the Focus and Snip tiles hand the right
  address to the shell (recorded, not launched, in the self-test).
* In unit tests anywhere: the page's behaviour (drag, throttle, hold, optimistic toggles, each
  "not available" state), brightness snapping, which stored brightness applies, how several radios of
  one kind read as one.
* **Not verified:** actually changing the volume, the brightness or a radio on real hardware (the
  unit tests deliberately do not, because on a developer's laptop they would).
