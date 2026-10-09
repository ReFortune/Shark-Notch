# System stats

One page, "Stats": processor, memory, GPU and network with the last minute of history each, and the
battery. Switched on or off, and tuned, in `[stats]` of `config.toml`; remove `"stats"` from
`[modules] order` to hide the page.

```toml
[stats]
enabled = true
interval_secs = 1.0   # between readings while the page is on screen (0.5 to 5)
gpu = true            # read the GPU counters too; off hides the figure
net_bits = false      # Mbps instead of MB/s
```

## When anything is measured

**Only while the page is on screen.** The module host polls a module *only* while that module's page is
visible in the expanded notch; each poll just sends a "take a reading" request to a worker thread.
With the notch collapsed, on another page, hidden for a fullscreen app, locked or paused:

* nothing asks for a reading, so nothing is read. The worker thread sleeps in `recv()`; its one
  wake-up of its own comes about 5 s after the last reading, when it lets go of the counter query and
  the last reading, and then it sleeps again with no timer. The self-test checks this by counting the
  readings after closing the page (zero), and again when a fullscreen app takes over the screen while
  the page is still open (zero);
* the history is thrown away, so reopening starts a clean minute instead of a stale one with a gap.

A rate needs two readings, so the first request after the page opens takes a baseline, waits a
quarter of a second *on the worker thread* and takes the second; the first numbers therefore appear
about 250 ms after the page does. Later readings answer at once.

## Where each number comes from

| Tile | Source | What to know |
|------|--------|--------------|
| CPU | `GetSystemTimes`: busy share of all processors between two readings | System-wide average, like Task Manager's CPU graph. Recent Windows versions show a "utility" figure there that accounts for clock speed, so the two can differ by a few points. |
| Memory | `GlobalMemoryStatusEx`: total minus available physical memory | "Available" includes the standby cache, as in Task Manager's *In use*. |
| GPU | The "GPU Engine" performance counters (PDH), summed over processes per engine; the **busiest engine** is shown, as Task Manager's headline figure does | Needs Windows 10 1709+ and a WDDM 2.x driver. Some machines (and virtual machines) have no such counters: the tile then says "No GPU counters on this PC". The counters cost a little more than the rest, hence `gpu = false`. |
| Network | `GetIfTable2` byte counters summed over the **physical adapters that are up** | VPN, Hyper-V and WSL adapters carry the same bytes again and are left out. Wi-Fi and Ethernet together if both are up. The chart scale never drops below 128 KB/s so background noise does not look like traffic. |
| Battery | `GetSystemPowerStatus` | "left" is Windows' own estimate; a PC without a battery says so. Shows "saver on" when battery saver is. |
| Charger banner | `RegisterPowerSettingNotification` (power source and battery level), delivered to the app window | A banner when the charger is plugged in or removed and when the battery reaches 100 % on the charger. Event-driven: nothing is read between events. The first notification Windows sends is only the baseline. Never over a fullscreen app. `[stats] battery_hud = false` turns it off. Needs a battery to ever fire; not tested on a real one. |
| Devices strip | `SetupDiGetDevicePropertyW` on the `BTHENUM` / `BTHLE` nodes: battery `{104EA319-6EE2-4701-BD47-8DDBF425BBE5}` 2, connected `{83DA6326-97A6-4088-9453-A1923F573B29}` 15 | Connected devices only, up to three, with a percentage when Windows has one. Read at most every 8 s, only while the page is open. The battery is reported on a *service* node (a headset's hands-free one), so nodes are joined by hardware address. **Windows does not report an iPhone's battery this way** (checked on one machine: connected, no figure); [`[stats] devices = false`] removes the strip. |
| Claude Code activity | the session logs Claude Code already writes: `%USERPROFILE%\.claude\projects\**\*.jsonl` (or `%CLAUDE_CONFIG_DIR%`), watched with `FindFirstChangeNotificationW` | Used only to know when Claude Code is working, so the limits are not polled while it is idle. Nothing is shown for it. Only the message id, the time and the counters are taken from a line; no text is kept, nothing is sent anywhere. `[stats] ai_usage = false` stops the watcher. |
| Claude plan limits (opt-in) | `GET https://api.anthropic.com/api/oauth/usage` with the login token Claude Code keeps in `~/.claude/.credentials.json` | A **line on this page** with the two bars (`5 h 42% · 2h10m`, `Week 18% · 3d2h`), asked for when the page opens and about once a minute while it stays open (nothing is shown anywhere else and nothing permanently); and a pop-up banner ("Claude 5-hour limit, 92% used, resets in 1 h 20 min", a bar) when the 5-hour or the weekly limit passes **80 %** and again at **95 %**; once per level per window, silent otherwise and never over a fullscreen app. Nothing is displayed permanently. Asked for when Claude Code starts working, at most once a minute. **Off by default** (`[stats] ai_limits`): it reads that token and sends it to Anthropic. The endpoint is **not documented for third parties** and its answer shape was assumed (`five_hour` / `seven_day` with `utilization` and `resets_at`), not confirmed against a real answer. The token is never logged or stored; an expired one is not refreshed (use Claude Code once). |

Nothing needs elevation, a driver or an install; nothing is written anywhere.

## Cost

Measured by the self-test on a CI virtual machine (software renderer, so the *drawing* share is not
representative of a laptop GPU): see `docs/PERFORMANCE.md` for the figure with the page open and
sampling once a second. Closed, the cost is zero. What the open page does each second: four small
syscalls, one network-table read (and one PDH query for the GPU), one event to the UI thread and one
redraw of a page of eight shapes.

## Not here, on purpose

* **Per-core load, temperatures, fan speeds, disk throughput.** Not in the brief. Temperatures in
  particular have no reliable, driver-free Windows API (the WMI thermal zones are not what people
  expect); reading them properly means vendor drivers, which this project avoids.
* **GPU memory and per-process GPU use.** Possible with the same counters; not needed for a glance.

## Verified, and not

* Verified on a real Windows machine in CI: every reading returns plausible values, the sampler
  answers a request with one snapshot and sends nothing unasked, stopping is prompt even during the
  baseline pause, nothing is sampled after the page is closed, and the page draws its tiles and charts.
* **Not verified:** the GPU counters on a real GPU (the CI machine has none, so only the "absent"
  path ran), the battery figures on a real battery, and network speeds against a real download.
