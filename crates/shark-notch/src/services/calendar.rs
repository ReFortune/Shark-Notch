//! Calendar feeds as a bus producer: reads the configured ICS links/files, expands the events of a
//! window around today (recurrences included) and publishes one `Calendar` event.
//!
//! One worker thread, and only while the calendar module is enabled **and** a feed is configured.
//! It sleeps in `recv_timeout` until the next refresh is due (`calendar.refresh_minutes`), a refresh
//! is asked for (the page opened with stale data), or the configuration changes. It does no work
//! while a fullscreen app is in front; a refresh that came due meanwhile happens when it ends.
//! The feed link is a secret: it is never written to the log, only fetched over `https`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notch_core::bus::BusSender;
use notch_core::config::CalendarCfg;
use notch_core::events::{CalEvent, CalendarData, EventKind, Source};
use notch_core::ics::{Calendar, FeedLocation, parse_feed_location};
use notch_core::module::CalCmd;

use crate::win::http;
use crate::win::tz::WinZone;

/// Feeds larger than this are refused.
const MAX_FEED_BYTES: usize = 8 << 20;
/// Events kept per refresh, and the window they are read for.
const MAX_EVENTS: usize = 6_000;
const DAYS_BEFORE: i64 = 40;
const DAYS_AFTER: i64 = 130;
/// A refresh asked for within this long of the previous one is ignored.
const MIN_GAP: Duration = Duration::from_secs(20);

enum Req {
    Cmd(CalCmd),
    Configure(CalendarCfg),
    Suspend(bool),
    Quit,
}

pub struct CalendarService {
    tx: Sender<Req>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

impl CalendarService {
    pub fn start(cfg: CalendarCfg, bus: BusSender) -> Option<CalendarService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let thread = std::thread::Builder::new()
            .name("calendar".into())
            .stack_size(1 << 20)
            .spawn(move || {
                run(rx, bus, cfg);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the calendar worker: {e}"))
            .ok()?;
        Some(CalendarService {
            tx,
            thread: Some(thread),
            done,
        })
    }

    pub fn command(&self, cmd: CalCmd) {
        let _ = self.tx.send(Req::Cmd(cmd));
    }

    pub fn configure(&self, cfg: CalendarCfg) {
        let _ = self.tx.send(Req::Configure(cfg));
    }

    pub fn suspend(&self, on: bool) {
        let _ = self.tx.send(Req::Suspend(on));
    }

    /// Stop the worker; one that is in the middle of a download is left to finish by itself.
    pub fn stop(mut self) {
        let _ = self.tx.send(Req::Quit);
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(300);
            while !self.done.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.done.load(Ordering::Acquire) {
                let _ = h.join();
            }
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn run(rx: Receiver<Req>, bus: BusSender, mut cfg: CalendarCfg) {
    let mut suspended = false;
    // When the next refresh is due; `None` = as soon as possible.
    let mut due: Option<Instant> = Some(Instant::now());
    let mut last: Option<Instant> = None;
    loop {
        // With nothing to fetch, or while a game is in front, only a message can wake the thread.
        let wait = if cfg.feeds.is_empty() || suspended {
            None
        } else {
            due.map(|d| d.saturating_duration_since(Instant::now()))
        };
        let msg = match wait {
            Some(w) => rx.recv_timeout(w),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Req::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Req::Suspend(on)) => suspended = on,
            Ok(Req::Configure(new)) => {
                if new.feeds != cfg.feeds {
                    due = Some(Instant::now());
                }
                cfg = new;
            }
            Ok(Req::Cmd(CalCmd::Refresh)) => {
                if last.is_none_or(|l| l.elapsed() >= MIN_GAP) {
                    due = Some(Instant::now());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if suspended || cfg.feeds.is_empty() {
            continue;
        }
        if due.is_some_and(|d| d <= Instant::now()) {
            let data = refresh(&cfg);
            last = Some(Instant::now());
            due = Some(Instant::now() + Duration::from_secs(u64::from(cfg.refresh_minutes) * 60));
            bus.send(Source::Local, EventKind::Calendar(Arc::new(data)));
        }
    }
}

/// Read every feed and expand its events around today.
fn refresh(cfg: &CalendarCfg) -> CalendarData {
    let now = now_unix();
    let (from, to) = (now - DAYS_BEFORE * 86_400, now + DAYS_AFTER * 86_400);
    let zone = WinZone;
    let mut events: Vec<CalEvent> = Vec::new();
    let (mut failed, mut error) = (0u32, None::<Arc<str>>);
    for (i, feed) in cfg.feeds.iter().enumerate().take(8) {
        match read_feed(feed) {
            Ok(text) => {
                let cal = Calendar::parse(&text);
                let room = MAX_EVENTS.saturating_sub(events.len());
                events.extend(
                    cal.occurrences(from, to, &zone, room)
                        .into_iter()
                        .map(|o| to_event(o, i as u8)),
                );
                crate::debug!(
                    "calendar: feed {i} gave {} event(s) in the window",
                    events.len()
                );
            }
            Err(e) => {
                failed += 1;
                // Never log the link itself (it is a secret); the reason is enough.
                crate::warn!("calendar: feed {i} could not be read: {e}");
                error.get_or_insert_with(|| e.into());
            }
        }
    }
    events.sort_by_key(|e| (e.start_utc, e.title.clone()));
    CalendarData {
        events,
        fetched_unix: now,
        feeds: cfg.feeds.len() as u32,
        failed,
        error,
    }
}

fn to_event(o: notch_core::ics::Occurrence, feed: u8) -> CalEvent {
    CalEvent {
        title: o.title.into(),
        location: o.location.into(),
        start_utc: o.start_utc,
        end_utc: o.end_utc,
        start_local: o.start_local,
        end_local: o.end_local,
        all_day: o.all_day,
        join_url: o.join_url.map(Into::into),
        feed,
    }
}

/// The text of one feed: fetched over `https`, or read from a file.
fn read_feed(feed: &str) -> Result<String, String> {
    let bytes = match parse_feed_location(feed) {
        FeedLocation::Https { host, port, path } => {
            http::get_https(&host, port, &path, MAX_FEED_BYTES)?
        }
        FeedLocation::File(path) => {
            let path = std::path::Path::new(&path);
            let meta =
                std::fs::metadata(path).map_err(|e| format!("cannot read the file ({e})"))?;
            if meta.len() as usize > MAX_FEED_BYTES {
                return Err("the file is too large".into());
            }
            std::fs::read(path).map_err(|e| format!("cannot read the file ({e})"))?
        }
        FeedLocation::Rejected(why) => return Err(why.to_string()),
    };
    // ICS is UTF-8 (some exporters add a byte-order mark; a few use Latin-1 and are read lossily).
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notch_core::bus::{Bus, Waker};
    use notch_core::ics::LocalZone;

    struct NoWake;
    impl Waker for NoWake {
        fn wake(&self) {}
    }

    fn write_feed(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shark-notch-cal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn feed_text() -> String {
        // An event 2 hours from now, with a call link, and a daily series.
        let now = now_unix();
        let (y, mo, d, h, mi, s) = notch_core::civil::civil_from_unix(now + 7200);
        let stamp = format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z");
        format!(
            "\u{feff}BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
             BEGIN:VEVENT\r\nUID:one\r\nDTSTART:{stamp}\r\nDURATION:PT30M\r\nSUMMARY:Selftest meeting\r\n\
             LOCATION:https://meet.google.com/abc-defg-hij\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:two\r\nDTSTART:{stamp}\r\nDURATION:PT15M\r\nRRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Daily\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n"
        )
    }

    #[test]
    fn a_local_feed_is_read_expanded_and_sorted() {
        let path = write_feed("a.ics", &feed_text());
        let cfg = CalendarCfg {
            feeds: vec![path.to_string_lossy().into_owned()],
            ..CalendarCfg::default()
        };
        let data = refresh(&cfg);
        assert_eq!((data.feeds, data.failed), (1, 0), "{:?}", data.error);
        assert_eq!(data.events.len(), 4, "one meeting + three daily instances");
        assert!(
            data.events
                .windows(2)
                .all(|w| w[0].start_utc <= w[1].start_utc)
        );
        let meeting = data
            .events
            .iter()
            .find(|e| &*e.title == "Selftest meeting")
            .unwrap();
        assert_eq!(
            meeting.join_url.as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
        assert_eq!(meeting.end_utc - meeting.start_utc, 1800);
        // The local time is the zone's view of the UTC time.
        assert_eq!(meeting.start_local, WinZone.utc_to_local(meeting.start_utc));
        assert!(data.fetched_unix > 0);
    }

    #[test]
    fn bad_feeds_are_counted_and_described_without_breaking_the_good_ones() {
        let good = write_feed("good.ics", &feed_text());
        let cfg = CalendarCfg {
            feeds: vec![
                "http://example.com/plain.ics".into(),
                good.to_string_lossy().into_owned(),
                r"C:\definitely\not\there.ics".into(),
            ],
            ..CalendarCfg::default()
        };
        let data = refresh(&cfg);
        assert_eq!((data.feeds, data.failed), (3, 2));
        assert!(!data.events.is_empty(), "the good feed still shows");
        let err = data.error.unwrap();
        assert!(
            err.contains("https"),
            "the first failure is explained: {err}"
        );
    }

    #[test]
    fn the_service_publishes_on_start_and_on_demand_and_sleeps_without_feeds() {
        let path = write_feed("svc.ics", &feed_text());
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = CalendarService::start(
            CalendarCfg {
                feeds: vec![path.to_string_lossy().into_owned()],
                ..CalendarCfg::default()
            },
            tx,
        )
        .unwrap();
        let wait_for = |bus: &mut Bus, n: usize| {
            let mut got = Vec::new();
            let until = Instant::now() + Duration::from_secs(10);
            while got.len() < n && Instant::now() < until {
                bus.drain(&mut got);
                std::thread::sleep(Duration::from_millis(10));
            }
            got
        };
        let first = wait_for(&mut bus, 1);
        assert!(
            matches!(first[0].kind, EventKind::Calendar(_)),
            "a refresh at start-up"
        );
        // A second request right away is ignored (rate limit)...
        svc.command(CalCmd::Refresh);
        std::thread::sleep(Duration::from_millis(300));
        let mut extra = Vec::new();
        bus.drain(&mut extra);
        assert!(extra.is_empty(), "refreshes are rate-limited");
        // ...and a new feed list refreshes at once.
        let other = write_feed("svc2.ics", &feed_text());
        svc.configure(CalendarCfg {
            feeds: vec![other.to_string_lossy().into_owned()],
            ..CalendarCfg::default()
        });
        let again = wait_for(&mut bus, 1);
        assert_eq!(again.len(), 1);
        svc.stop();
    }

    #[test]
    fn a_byte_order_mark_is_ignored() {
        let path = write_feed("bom.ics", &feed_text());
        let text = read_feed(&path.to_string_lossy()).unwrap();
        assert!(text.starts_with("BEGIN:VCALENDAR"));
    }
}
