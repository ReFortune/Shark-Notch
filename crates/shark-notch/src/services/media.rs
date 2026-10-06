//! The system media session (SMTC) as a bus producer.
//!
//! One small worker thread owns every WinRT object. It subscribes to the session manager's and the
//! current session's change events (so it sleeps in `recv` until something actually changes),
//! reads the session into a [`MediaSnapshot`], decodes the album art with WIC, and sends
//! `MediaChanged` on the bus. Playback commands from the UI arrive on the same channel and are
//! fired without waiting for the player to answer. Nothing here ever blocks the UI thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notch_core::bus::BusSender;
use notch_core::draw::ImageId;
use notch_core::events::{EventKind, MediaSnapshot, Source};
use notch_core::image::ImageCache;
use notch_core::module::MediaCmd;
use notch_core::modules::media::friendly_app_name;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    CurrentSessionChangedEventArgs, GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as Props,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    MediaPropertiesChangedEventArgs, PlaybackInfoChangedEventArgs, SessionsChangedEventArgs,
    TimelinePropertiesChangedEventArgs,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

use crate::win::imaging;

/// Album art is decoded to at most this many pixels on a side (it is drawn at ~92 DIPs).
const ART_EDGE: u32 = 192;
/// Reject absurd thumbnails before reading them into memory.
const MAX_ART_BYTES: u64 = 16 << 20;
/// SMTC fires several events per track change; wait this long to coalesce them into one read.
const COALESCE: Duration = Duration::from_millis(40);
/// A new track's first snapshot waits this long for its art, so the peek shows the cover at once.
const ART_HOLD: Duration = Duration::from_millis(350);
/// Retry schedule for a thumbnail that was not there yet.
const ART_RETRY: [Duration; 3] = [
    Duration::from_millis(120),
    Duration::from_millis(900),
    Duration::from_millis(2500),
];
/// 100 ns ticks per millisecond.
const TICKS_PER_MS: i64 = 10_000;

enum Req {
    Cmd(MediaCmd),
    /// Something about the current session changed.
    Changed,
    /// The set of sessions / the current session changed.
    SessionChanged,
    Suspend(bool),
    Quit,
}

pub struct MediaService {
    tx: Sender<Req>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

impl MediaService {
    pub fn start(bus: BusSender, images: Arc<ImageCache>) -> Option<MediaService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let (tx2, done2) = (tx.clone(), done.clone());
        let thread = std::thread::Builder::new()
            .name("media-smtc".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(rx, tx2, bus, images);
                done2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the media worker: {e}"))
            .ok()?;
        Some(MediaService {
            tx,
            thread: Some(thread),
            done,
        })
    }

    pub fn command(&self, cmd: MediaCmd) {
        let _ = self.tx.send(Req::Cmd(cmd));
    }

    pub fn suspend(&self, on: bool) {
        let _ = self.tx.send(Req::Suspend(on));
    }

    /// Stop the worker. Waits briefly; if a player is hanging a WinRT call the thread is left to
    /// finish on its own rather than freezing the UI.
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

struct ComGuard(bool);

impl ComGuard {
    fn mta() -> ComGuard {
        ComGuard(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { RoUninitialize() };
        }
    }
}

struct Attached {
    session: Session,
    tokens: [Option<i64>; 3],
}

struct ArtState {
    id: ImageId,
    accent: Option<[u8; 3]>,
}

struct Worker {
    rx: Receiver<Req>,
    tx: Sender<Req>,
    bus: BusSender,
    images: Arc<ImageCache>,
    mgr: Manager,
    mgr_tokens: [Option<i64>; 2],
    session: Option<Attached>,
    /// Identity of the track the current art belongs to.
    art_key: String,
    art: Option<ArtState>,
    art_tries: usize,
    /// When to read again (coalescing, art retries).
    wake_at: Option<Instant>,
    /// A new track whose art has not arrived yet: emit by this time regardless.
    hold_until: Option<Instant>,
    last_sent: Option<(MediaSnapshot, Instant)>,
    paused: bool,
    dirty: bool,
    need_attach: bool,
    read_failures: u32,
}

fn run(rx: Receiver<Req>, tx: Sender<Req>, bus: BusSender, images: Arc<ImageCache>) {
    let _com = ComGuard::mta();
    let mgr = match Manager::RequestAsync().and_then(|op| op.join()) {
        Ok(m) => m,
        Err(e) => {
            crate::warn!(
                "media: the system media session manager is unavailable ({e}); the media page stays off"
            );
            // Nothing to do but wait to be told to stop.
            while let Ok(r) = rx.recv() {
                if matches!(r, Req::Quit) {
                    break;
                }
            }
            return;
        }
    };
    let mut w = Worker {
        rx,
        tx,
        bus,
        images,
        mgr,
        mgr_tokens: [None, None],
        session: None,
        art_key: String::new(),
        art: None,
        art_tries: 0,
        wake_at: None,
        hold_until: None,
        last_sent: None,
        paused: false,
        dirty: false,
        need_attach: false,
        read_failures: 0,
    };
    w.subscribe_manager();
    w.attach();
    w.refresh();
    w.event_loop();
    w.shutdown();
}

impl Worker {
    fn subscribe_manager(&mut self) {
        let tx = self.tx.clone();
        let h = TypedEventHandler::<Manager, CurrentSessionChangedEventArgs>::new(move |_, _| {
            let _ = tx.send(Req::SessionChanged);
            Ok(())
        });
        self.mgr_tokens[0] = self.mgr.CurrentSessionChanged(&h).ok();
        let tx = self.tx.clone();
        let h = TypedEventHandler::<Manager, SessionsChangedEventArgs>::new(move |_, _| {
            let _ = tx.send(Req::SessionChanged);
            Ok(())
        });
        self.mgr_tokens[1] = self.mgr.SessionsChanged(&h).ok();
    }

    fn detach(&mut self) {
        if let Some(a) = self.session.take() {
            // Removing handlers from a session that has already closed can fail; that is fine.
            if let Some(t) = a.tokens[0] {
                let _ = a.session.RemoveMediaPropertiesChanged(t);
            }
            if let Some(t) = a.tokens[1] {
                let _ = a.session.RemovePlaybackInfoChanged(t);
            }
            if let Some(t) = a.tokens[2] {
                let _ = a.session.RemoveTimelinePropertiesChanged(t);
            }
        }
    }

    /// Subscribe to the manager's current session (if there is one).
    fn attach(&mut self) {
        self.detach();
        let Ok(session) = self.mgr.GetCurrentSession() else {
            return;
        };
        let changed = |tx: Sender<Req>| {
            move || {
                let _ = tx.send(Req::Changed);
            }
        };
        let (c1, c2, c3) = (
            changed(self.tx.clone()),
            changed(self.tx.clone()),
            changed(self.tx.clone()),
        );
        let t0 = session
            .MediaPropertiesChanged(
                &TypedEventHandler::<Session, MediaPropertiesChangedEventArgs>::new(move |_, _| {
                    c1();
                    Ok(())
                }),
            )
            .ok();
        let t1 = session
            .PlaybackInfoChanged(
                &TypedEventHandler::<Session, PlaybackInfoChangedEventArgs>::new(move |_, _| {
                    c2();
                    Ok(())
                }),
            )
            .ok();
        let t2 = session
            .TimelinePropertiesChanged(&TypedEventHandler::<
                Session,
                TimelinePropertiesChangedEventArgs,
            >::new(move |_, _| {
                c3();
                Ok(())
            }))
            .ok();
        self.session = Some(Attached {
            session,
            tokens: [t0, t1, t2],
        });
    }

    fn event_loop(&mut self) {
        loop {
            let msg = match self.wake_at {
                Some(t) => self
                    .rx
                    .recv_timeout(t.saturating_duration_since(Instant::now())),
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(Req::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Req::Suspend(on)) => {
                    self.paused = on;
                    if !on && self.dirty {
                        self.dirty = false;
                        self.soon(Duration::ZERO);
                    }
                }
                Ok(Req::SessionChanged) => {
                    self.need_attach = true;
                    self.soon(COALESCE);
                }
                Ok(Req::Changed) | Ok(Req::Cmd(MediaCmd::Refresh)) => self.soon(COALESCE),
                Ok(Req::Cmd(cmd)) => self.execute(cmd),
                Err(RecvTimeoutError::Timeout) => {
                    self.wake_at = None;
                    if self.paused {
                        self.dirty = true;
                        continue;
                    }
                    if std::mem::take(&mut self.need_attach) {
                        self.attach();
                    }
                    self.refresh();
                }
            }
        }
    }

    /// Schedule a read after `d` (never *later* than an already scheduled one).
    fn soon(&mut self, d: Duration) {
        let t = Instant::now() + d;
        self.wake_at = Some(self.wake_at.map_or(t, |w| w.min(t)));
    }

    fn shutdown(&mut self) {
        self.detach();
        if let Some(t) = self.mgr_tokens[0].take() {
            let _ = self.mgr.RemoveCurrentSessionChanged(t);
        }
        if let Some(t) = self.mgr_tokens[1].take() {
            let _ = self.mgr.RemoveSessionsChanged(t);
        }
        if let Some(a) = self.art.take() {
            self.images.remove(a.id);
        }
    }

    /// Fire a playback command. The player's answer arrives as a change event, so nothing waits.
    fn execute(&mut self, cmd: MediaCmd) {
        let Some(a) = &self.session else { return };
        match cmd {
            MediaCmd::PlayPause => {
                let _ = a.session.TryTogglePlayPauseAsync();
            }
            MediaCmd::Next => {
                let _ = a.session.TrySkipNextAsync();
            }
            MediaCmd::Previous => {
                let _ = a.session.TrySkipPreviousAsync();
            }
            MediaCmd::SeekTo(ms) => {
                let start = a
                    .session
                    .GetTimelineProperties()
                    .and_then(|t| t.StartTime())
                    .map_or(0, |s| s.Duration);
                let _ = a
                    .session
                    .TryChangePlaybackPositionAsync(start + ms as i64 * TICKS_PER_MS);
            }
            MediaCmd::Refresh => {}
        }
        // The events will come, but a read shortly after makes the UI settle even for players that
        // forget to raise them.
        self.soon(Duration::from_millis(250));
    }

    /// Read the session and publish it (if it changed).
    fn refresh(&mut self) {
        let session = self.session.as_ref().map(|a| a.session.clone());
        let snap = match session.map(|s| self.read(&s)) {
            Some(Ok(Some(s))) => {
                self.read_failures = 0;
                s
            }
            Some(Ok(None)) => return, // holding for art
            Some(Err(e)) => {
                // The session may be closing, or the player mid-update: look again shortly, and
                // only after a few failures conclude that there is nothing to show.
                crate::debug!("media: read failed: {e}");
                self.read_failures += 1;
                self.need_attach = true;
                if self.read_failures < 3 {
                    self.soon(Duration::from_millis(300));
                    return;
                }
                MediaSnapshot::default()
            }
            None => MediaSnapshot::default(),
        };
        self.publish(snap);
    }

    fn publish(&mut self, snap: MediaSnapshot) {
        if let Some((prev, at)) = &self.last_sent
            && same_enough(prev, &snap, at.elapsed())
        {
            return;
        }
        self.last_sent = Some((snap.clone(), Instant::now()));
        self.bus
            .send(Source::Local, EventKind::MediaChanged(Arc::new(snap)));
    }

    /// `Ok(None)` = a new track is waiting briefly for its album art.
    fn read(&mut self, session: &Session) -> windows::core::Result<Option<MediaSnapshot>> {
        let aumid = session.SourceAppUserModelId()?.to_string();
        let props = session.TryGetMediaPropertiesAsync()?.join()?;
        let (title, artist, album) = (
            props.Title()?.to_string(),
            props.Artist()?.to_string(),
            props.AlbumTitle()?.to_string(),
        );
        let playback = session.GetPlaybackInfo()?;
        let playing = playback.PlaybackStatus()? == Status::Playing;
        let controls = playback.Controls()?;
        let timeline = session.GetTimelineProperties()?;

        // Position as of *now*: SMTC reports it as of `LastUpdatedTime`.
        let (start, end, pos, updated) = (
            timeline.StartTime()?.Duration,
            timeline.EndTime()?.Duration,
            timeline.Position()?.Duration,
            timeline.LastUpdatedTime()?.UniversalTime,
        );
        let (position_ms, duration_ms) =
            media_times(start, end, pos, updated, playing, now_filetime_ticks());

        let key = format!("{aumid}\u{1}{title}\u{1}{artist}\u{1}{album}");
        let new_track = key != self.art_key;
        if new_track {
            if let Some(a) = self.art.take() {
                self.images.remove(a.id);
            }
            self.art_key = key;
            self.art_tries = 0;
            self.hold_until = None;
        }
        if self.art.is_none() && self.art_tries <= ART_RETRY.len() {
            self.art_tries += 1;
            match fetch_art(&props) {
                Some((data, accent)) => {
                    if let Some(id) = self.images.put(data) {
                        self.art = Some(ArtState { id, accent });
                    }
                }
                None => {
                    if let Some(d) = ART_RETRY.get(self.art_tries - 1) {
                        self.soon(*d);
                    }
                }
            }
        }
        if self.art.is_none() && new_track && !title.is_empty() {
            // Give the cover a moment: a peek that pops up and then grows an image looks cheap.
            let until = *self.hold_until.get_or_insert(Instant::now() + ART_HOLD);
            if Instant::now() < until {
                return Ok(None);
            }
        }
        self.hold_until = None;

        Ok(Some(MediaSnapshot {
            app: friendly_app_name(&aumid).into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            playing,
            position_ms,
            duration_ms,
            art: self.art.as_ref().map_or(0, |a| a.id.0),
            accent: self.art.as_ref().and_then(|a| a.accent),
            can_play_pause: controls.IsPlayPauseToggleEnabled().unwrap_or(false)
                || controls.IsPlayEnabled().unwrap_or(false)
                || controls.IsPauseEnabled().unwrap_or(false),
            can_next: controls.IsNextEnabled().unwrap_or(false),
            can_prev: controls.IsPreviousEnabled().unwrap_or(false),
            can_seek: controls.IsPlaybackPositionEnabled().unwrap_or(false),
        }))
    }
}

/// Read the session's thumbnail and decode it (on this worker thread).
fn fetch_art(props: &Props) -> Option<(notch_core::image::ImageData, Option<[u8; 3]>)> {
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.join().ok()?;
    let bytes = imaging::read_stream(&stream, MAX_ART_BYTES)?;
    let img = imaging::decode(&bytes, ART_EDGE)?;
    let c = img.dominant_color().to_rgba8();
    Some((img, Some([c[0], c[1], c[2]])))
}

/// Current time as Windows `DateTime` ticks (100 ns since 1601-01-01 UTC).
fn now_filetime_ticks() -> i64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() as i64 + 11_644_473_600) * 10_000_000 + i64::from(d.subsec_nanos() / 100)
}

/// `(position_ms, duration_ms)` from SMTC timeline values (all in 100 ns ticks). While playing the
/// reported position is advanced by the time since `updated`; a nonsensical `updated` (some
/// browsers send the year 1) is ignored. A duration over a day means "live / unknown" (0).
pub fn media_times(
    start: i64,
    end: i64,
    pos: i64,
    updated: i64,
    playing: bool,
    now: i64,
) -> (u64, u64) {
    let mut p = pos - start;
    if playing {
        let elapsed = now - updated;
        if (0..86_400 * 10_000_000i64).contains(&elapsed) {
            p += elapsed;
        }
    }
    let dur = (end - start).max(0) / TICKS_PER_MS;
    let dur = if dur > 24 * 3600 * 1000 {
        0
    } else {
        dur as u64
    };
    let mut pos_ms = (p.max(0) / TICKS_PER_MS) as u64;
    if dur > 0 {
        pos_ms = pos_ms.min(dur);
    }
    (pos_ms, dur)
}

/// Is `new` the same state as `old`, given that `old` was sent `age` ago? A playing track's
/// position advances by itself, so only drift beyond ~1.2 s counts as a change.
fn same_enough(old: &MediaSnapshot, new: &MediaSnapshot, age: Duration) -> bool {
    let expected = if old.playing {
        old.position_ms + age.as_millis() as u64
    } else {
        old.position_ms
    };
    old.same_track(new)
        && old.playing == new.playing
        && old.art == new.art
        && old.duration_ms == new.duration_ms
        && (old.can_play_pause, old.can_next, old.can_prev, old.can_seek)
            == (new.can_play_pause, new.can_next, new.can_prev, new.can_seek)
        && expected.abs_diff(new.position_ms) < 1200
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 10_000_000;

    #[test]
    fn position_advances_from_the_last_update_while_playing() {
        let now = 1_000 * S;
        // Reported 30 s in, updated 5 s ago, playing: 35 s now.
        let (p, d) = media_times(0, 200 * S, 30 * S, now - 5 * S, true, now);
        assert_eq!((p, d), (35_000, 200_000));
        // Paused: exactly what was reported.
        assert_eq!(
            media_times(0, 200 * S, 30 * S, now - 5 * S, false, now).0,
            30_000
        );
    }

    #[test]
    fn implausible_update_times_are_ignored() {
        let now = 1_000 * S;
        let year_one = 0;
        let now_2026 = 134_000_000_000_000_000;
        assert_eq!(
            media_times(0, 200 * S, 30 * S, year_one, true, now_2026).0,
            30_000,
            "year-1 timestamp: no elapsed credit"
        );
        assert_eq!(
            media_times(0, 200 * S, 30 * S, now + 50 * S, true, now).0,
            30_000,
            "update in the future"
        );
    }

    #[test]
    fn position_is_relative_to_the_start_time_and_clamped() {
        let now = 100 * S;
        let (p, d) = media_times(10 * S, 70 * S, 40 * S, now, false, now);
        assert_eq!((p, d), (30_000, 60_000));
        assert_eq!(
            media_times(0, 60 * S, 90 * S, now, false, now).0,
            60_000,
            "never past the end"
        );
        assert_eq!(
            media_times(0, 60 * S, -5 * S, now, false, now).0,
            0,
            "never negative"
        );
    }

    #[test]
    fn live_and_unknown_durations_become_zero() {
        let now = 100 * S;
        assert_eq!(media_times(0, 0, 0, now, true, now).1, 0);
        assert_eq!(
            media_times(0, 90_000 * S, 5 * S, now, false, now).1,
            0,
            "over a day: a live stream"
        );
        assert_eq!(
            media_times(5 * S, S, 5 * S, now, false, now).1,
            0,
            "end before start"
        );
    }

    #[test]
    fn filetime_now_is_in_the_right_epoch() {
        // 2020-01-01 as ticks since 1601 is 132_223_104_000_000_000.
        assert!(now_filetime_ticks() > 132_223_104_000_000_000);
    }

    fn snap(title: &str, playing: bool, pos: u64) -> MediaSnapshot {
        MediaSnapshot {
            app: "A".into(),
            title: title.into(),
            playing,
            position_ms: pos,
            duration_ms: 100_000,
            can_play_pause: true,
            ..Default::default()
        }
    }

    #[test]
    fn an_unchanged_session_is_not_resent() {
        let a = snap("T", true, 10_000);
        assert!(
            same_enough(&a, &snap("T", true, 13_000), Duration::from_secs(3)),
            "advanced by itself"
        );
        assert!(
            !same_enough(&a, &snap("T", true, 40_000), Duration::from_secs(3)),
            "the user seeked"
        );
        assert!(
            !same_enough(&a, &snap("T", false, 13_000), Duration::from_secs(3)),
            "paused"
        );
        assert!(
            !same_enough(&a, &snap("U", true, 13_000), Duration::from_secs(3)),
            "new track"
        );
        let paused = snap("T", false, 10_000);
        assert!(
            same_enough(&paused, &snap("T", false, 10_500), Duration::from_secs(60)),
            "paused position is static"
        );
    }
}
