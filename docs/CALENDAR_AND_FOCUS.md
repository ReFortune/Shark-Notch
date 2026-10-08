# Calendar and Focus (Pomodoro + tasks)

Two pages, each switched on or off on its own (`[calendar]`, `[pomodoro]` in `config.toml`; remove a
name from `[modules] order` to hide its page).

## Calendar

A month grid with the selected day's agenda beside it, a countdown **chip** in the collapsed pill
for the next meeting, and a **banner with a Join button** shortly before it starts.

### Connecting a calendar

Shark Notch reads standard **ICS feeds**. It does not sign in to anything and has no access to your
account: it fetches one link (or reads one file) and shows what is in it.

| Where | How to get the link |
|-------|--------------------|
| Google Calendar | Settings → the calendar → *Integrate calendar* → **Secret address in iCal format** |
| Outlook.com / Microsoft 365 | Settings → Calendar → *Shared calendars* → *Publish a calendar* → copy the **ICS** link (some organisations switch publishing off) |
| iCloud | Calendar → share the calendar as a **public** calendar → copy the `webcal://` link (anyone with the link can read it) |
| Anything else | an `.ics` file on disk (a synced folder works well): put its path in `feeds` |

```toml
[calendar]
feeds = ["https://calendar.google.com/calendar/ical/…/basic.ics", 'C:\Users\me\work.ics']
```

* `https://` and `webcal://` links are fetched with the system's own HTTPS stack (Windows' TLS,
  certificate store and proxy settings). **Plain `http://` is refused**: the link is a secret that
  grants read access, so it never travels unencrypted. The link is never written to the log. Treat
  `config.toml` as private.
* The feeds are read when the app starts, every `refresh_minutes` (default 30), and when the page is
  opened while the data is older than ten minutes. Nothing runs between those moments, and nothing
  runs while a fullscreen app is in front.
* The window read is 40 days back to 130 days ahead (so you can browse a few months each way).

### What is understood

Timed events in UTC, in a named time zone (using the zone rules **inside the feed**), or "floating";
all-day and multi-day events; recurrence (`RRULE` daily / weekly / monthly / yearly with interval,
count, until, `BYDAY` including "second Tuesday" / "last Friday", `BYMONTHDAY` including negatives,
`BYMONTH`, `BYSETPOS`), extra dates, excluded dates, moved or cancelled single instances; cancelled
events are skipped. A recurring meeting keeps its local time across daylight-saving changes.

Not understood (the event then shows once, at its first date): `BYWEEKNO`, `BYYEARDAY`, `BYHOUR`,
`BYMINUTE`, `BYSECOND` and sub-daily repeats. A time zone name that the feed does not define is read as your local time.

### The Join button

Links on the usual conferencing hosts (Google Meet, Zoom, Microsoft Teams, Webex, Whereby, Jitsi,
GoTo, BlueJeans, Chime, Around) are found in the event's conference property, link, location or
description. **Only those hosts are ever offered**: invitations written by other people end up in
your feed, so an arbitrary link in an invitation is never a one-click action. Clicking opens the
link in your browser; nothing is joined automatically.

### Banners and the chip

* A banner (title, time, **Join** if there is a link) `alert_minutes` before an event starts (default
  5) and again as it starts; each is shown once. Clicking elsewhere on the banner opens the page.
* The pill shows a countdown for an event within `chip_minutes` (default 15), updating once a minute,
  until two minutes after it starts. While a chip is on the pill the GPU stack stays loaded (the chip
  is drawn with it); without one, an idle notch releases it.
* While a game or other fullscreen app is in front, no banner appears; if the meeting is still ahead
  when you come back, it is announced then.

### Wake-ups

The calendar needs the clock even while hidden. It asks the app to wake it at exactly the moments
something changes (the chip appears, the banner is due, the event starts, the chip's minute count
turns over) and at no other time.

## Focus: the timer and the task list

A 25-minute focus session (all lengths are configurable), then a short break, a long break after
every fourth session. Press play, pick a task, work.

* **The task you pick is the one the next finished session is credited to** (a count appears beside
  it). Click a task to make it current (click again to clear it), click the circle to tick it off,
  hover for ✕. Finished tasks sink to the bottom; *Clear done* removes them.
* **Add task** opens a text field. This is the only place the notch ever needs the keyboard, and it
  does not take it by itself: your click on the field asks for it, the window becomes focusable
  only for as long as you are typing. When you press Enter or Escape the window you were in gets the
  focus back; if you click another window instead, the field closes and that window keeps the focus. Typing and Ctrl+V work; **IME composition (Chinese, Japanese,
  Korean) is not supported** in this field. Nothing hooks the keyboard; the notch only sees keys
  while the field is open and the window has the focus.
* While running, the pill shows the minutes left. When a phase ends you get a banner (with a
  **Start** button if the next phase does not start by itself) and the system chime
  (`sound = false` silences it). Never while a fullscreen app is in front: it is announced when you
  come back.
* The timer follows the **wall clock**. A session that runs out while the PC sleeps is finished, and
  credited to its task, when the PC wakes. One that runs out while the app is *closed* comes back
  stopped at the start of its phase with nothing credited (the notch cannot know you were working);
  one still running when the app restarts carries on.

### What is stored

Tasks, their session counts, today's session count and the timer's state are saved to
`%LOCALAPPDATA%\SharkNotch\pomodoro.json` a moment after each change (written atomically; one file,
a few kilobytes, plain JSON you can read or delete). Nothing about the calendar is stored: events
live in memory and are fetched again.

## Honest status

Verified by tests on every change (portable core + the Windows CI runner): the ICS reader (including
daylight-saving transitions, recurrence edge cases and hostile input), join-link recognition, the
Pomodoro state machine, the task list, both pages' behaviour and drawing, the wake-up scheduling, the
persistence service, the feed service reading a real file through the real time-zone conversion, the
banner → Join click → link path, typing a task with surrogate pairs through the same handlers the
window uses, and that a task survives to `pomodoro.json`.

**Not verified:** fetching a real `https://` feed (CI has no calendar to point at; the WinHTTP client
is exercised only for its failure path), the first-time look on your monitor, `SetForegroundWindow`
behaviour on your system when you click *Add task* (Windows can refuse to move the focus to a
window; if it does, keystrokes do not reach the field and clicking another window closes it), and any
feed produced by an application whose quirks the RFC does not cover.
