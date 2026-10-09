//! Small typed events. Every event carries a [`Source`] so the UI can treat a clipboard item from
//! this PC and one from the iPhone — or a battery level from either device — identically.
//!
//! Payloads are deliberately tiny and cheap to clone (numbers, `Arc<str>`); bulky data such as
//! album art or image thumbnails lives in platform caches and is referenced by id.

use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// Produced on this PC.
    Local,
    /// Produced by the iPhone (via the LAN listener).
    Phone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipKind {
    Text,
    Link,
    Image,
    Files,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClipboardItem {
    pub id: u64,
    pub kind: ClipKind,
    /// A short single-line preview (never the full content).
    pub preview: Arc<str>,
    /// Key into the image cache of a small thumbnail (0 = none).
    pub thumb: u64,
    pub pinned: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    /// Unique per source: Windows ids are below 2^32, phone ids have bit 32 set.
    pub id: u64,
    pub app: Arc<str>,
    pub title: Arc<str>,
    pub body: Arc<str>,
    /// Key into the image cache of the app's logo (0 = none).
    pub icon: u64,
    /// `false` for notifications that were already waiting when the listener started: shown in the
    /// list, but never announced with a peek.
    pub fresh: bool,
    /// How old it already was when announced, in seconds (the backlog read at start-up).
    pub ago_secs: u32,
    /// Do-not-disturb / Focus is on: list it, but never announce it with a banner.
    pub quiet: bool,
}

/// One calendar event instance, with its times both as UTC instants and as the local wall clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalEvent {
    pub title: Arc<str>,
    pub location: Arc<str>,
    pub start_utc: i64,
    pub end_utc: i64,
    /// Local wall-clock seconds (as if UTC) of the start and end; all-day events are local midnights.
    pub start_local: i64,
    pub end_local: i64,
    pub all_day: bool,
    /// A link to the video call, only ever on a known conferencing host.
    pub join_url: Option<Arc<str>>,
    /// Index of the configured feed it came from.
    pub feed: u8,
}

/// What the calendar service knows after a refresh (events sorted by start).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CalendarData {
    pub events: Vec<CalEvent>,
    /// When it was fetched (UTC seconds); 0 = never.
    pub fetched_unix: i64,
    /// Feeds configured / feeds that could not be read in this refresh.
    pub feeds: u32,
    pub failed: u32,
    pub error: Option<Arc<str>>,
}

/// Which programs are using the microphone / camera right now (friendly names, sorted, unique).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrivacyState {
    pub mic: Vec<Arc<str>>,
    pub camera: Vec<Arc<str>>,
}

/// A download in progress (a browser's partial file that is growing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveDownload {
    pub name: Arc<str>,
    pub bytes: u64,
    pub speed_bps: u64,
}

/// A download that completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadDone {
    pub name: Arc<str>,
    /// Full path of the finished file (for "show in folder").
    pub path: Arc<str>,
    pub bytes: u64,
}

/// This computer's power state: the battery (the same `BatteryInfo` the iPhone's battery arrives
/// as) and what only the PC knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerStatus {
    pub battery: BatteryInfo,
    /// Connected to power (a full battery on the charger is plugged in but not charging).
    pub plugged: bool,
    /// Seconds of battery left at the current draw, when Windows can tell.
    pub secs_left: Option<u32>,
    /// The operating system's battery saver is on.
    pub saver: bool,
}

/// One reading of the machine's vital signs. A field is `None` when it could not be measured (the
/// first reading has no CPU percentage yet; a PC without a GPU counter has no GPU figure).
#[derive(Clone, Debug, PartialEq)]
pub struct StatsSnapshot {
    /// Busy share of the processor, 0..=100.
    pub cpu: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    /// Busiest GPU engine, 0..=100.
    pub gpu: Option<f32>,
    /// `(download, upload)` in bytes per second over the physical adapters.
    pub net: Option<(f64, f64)>,
    /// `None`: this PC has no battery.
    pub power: Option<PowerStatus>,
}

/// A radio (Wi-Fi, Bluetooth) as the command centre sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Radio {
    /// This PC has no such radio.
    Unavailable,
    /// Windows' "let apps control radios" privacy setting (or a policy) says no.
    Denied,
    /// The radio exists but is switched off in a way an app cannot change (a hardware switch,
    /// airplane mode, the device disabled).
    Disabled,
    Off,
    On,
}

/// The controls the command centre shows. A field is `None` when this PC (or this display) has no
/// such control, so the page can say so instead of offering something that does nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlState {
    /// Master output volume 0..=1 and whether it is muted.
    pub volume: Option<(f32, bool)>,
    /// Built-in panel brightness 0..=1.
    pub brightness: Option<f32>,
    pub wifi: Radio,
    pub bluetooth: Radio,
    /// Windows' do-not-disturb (focus assist) is on. Windows has no supported way to *change* it.
    pub dnd: Option<bool>,
}

/// The state of the iPhone link: where the PC listens and what it last heard.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PhoneLink {
    /// Addresses the phone can use, as `ip:port`.
    pub addrs: Vec<Arc<str>>,
    pub port: u16,
    /// Why the link is not listening (the port is taken, ...).
    pub error: Option<Arc<str>>,
    /// The last request that was accepted: when (unix seconds) and what ("clipboard", "file").
    pub last: Option<(i64, Arc<str>)>,
    /// Requests accepted / refused since the listener started.
    pub accepted: u32,
    pub refused: u32,
}

/// Something that arrived from the phone, for the app to put where it belongs. (The link never
/// calls other services itself: it only says what came in.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    /// Text or a link for the clipboard history.
    Text(Arc<str>),
    /// A file, already saved in the inbox folder, for the shelf.
    File(Arc<str>),
}

/// A value read back from the small persistent store (`None` = nothing saved under that key yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreItem {
    pub key: Arc<str>,
    pub data: Option<Arc<str>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    pub id: u64,
    pub name: Arc<str>,
    pub path: Arc<str>,
    pub size: u64,
    /// Key into the image cache of a thumbnail or file-type icon (0 = none).
    pub thumb: u64,
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatteryInfo {
    /// 0..=100.
    pub percent: u8,
    pub charging: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FocusInfo {
    /// e.g. "Work", "Sleep", "Do Not Disturb".
    pub name: Arc<str>,
    pub active: bool,
}

/// What the system's media session (Spotify, a browser tab, the Media Player...) is doing.
///
/// "No session" is the default value (empty title and app). Position is the position *at the moment
/// the snapshot was taken*; the media module extrapolates it while playing, so nothing needs to poll.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MediaSnapshot {
    /// Friendly name of the source app ("Spotify", "Chrome").
    pub app: Arc<str>,
    pub title: Arc<str>,
    pub artist: Arc<str>,
    pub album: Arc<str>,
    pub playing: bool,
    pub position_ms: u64,
    /// 0 = unknown / live stream.
    pub duration_ms: u64,
    /// Key into the platform's image cache for the album art (0 = none).
    pub art: u64,
    /// Dominant colour of the art as `[r, g, b]`, if there is art.
    pub accent: Option<[u8; 3]>,
    pub can_play_pause: bool,
    pub can_next: bool,
    pub can_prev: bool,
    pub can_seek: bool,
}

impl MediaSnapshot {
    /// Whether there is a media session worth showing.
    pub fn is_active(&self) -> bool {
        !self.title.is_empty() || !self.app.is_empty()
    }

    /// Same track (ignoring position, art and play state)?
    pub fn same_track(&self, o: &MediaSnapshot) -> bool {
        self.app == o.app
            && self.title == o.title
            && self.artist == o.artist
            && self.album == o.album
    }
}

/// What happened. Keep variants small; add payload types above rather than fields here.
#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
    /// The configuration was reloaded (modules re-read their section).
    ConfigChanged,
    /// Light/dark mode or accent colour changed.
    ThemeChanged,
    /// The shell stepped aside (`true`) or came back (`false`): fullscreen app, pause, lock, display off.
    Suspended(bool),
    /// A new clipboard entry, or an existing one that was copied again (same id: move to front).
    ClipboardItem(ClipboardItem),
    /// A clipboard entry was dropped (evicted or deleted).
    ClipboardRemoved(u64),
    Notification(Notification),
    FileDropped(Vec<FileEntry>),
    /// Something is being dragged over the notch's drop target (`true`), or left / was dropped (`false`).
    DragHover(bool),
    /// Battery level of the device named by the event's source (`Phone` = "PhoneBattery").
    Battery(BatteryInfo),
    FocusChanged(FocusInfo),
    MediaChanged(Arc<MediaSnapshot>),
    /// A fresh read of the calendar feeds.
    Calendar(Arc<CalendarData>),
    /// A value the store service read from disk (answer to `StoreCmd::Load`).
    StoreLoaded(StoreItem),
    /// The set of programs using the microphone / camera changed.
    Privacy(Arc<PrivacyState>),
    /// The downloads in progress (sent whenever they change, a couple of times a second at most).
    Downloads(Arc<Vec<ActiveDownload>>),
    DownloadDone(DownloadDone),
    /// A reading of the system's vital signs (answer to `StatsCmd::Sample`).
    Stats(Arc<StatsSnapshot>),
    /// The command centre's controls, as read (answer to `ControlCmd::Refresh` and to changes).
    Control(Arc<ControlState>),
    /// The state of the iPhone link.
    PhoneLink(Arc<PhoneLink>),
    /// Something the phone sent that belongs to another service (see [`Inbound`]).
    Inbound(Inbound),
}

/// Fieldless mirror of [`EventKind`], used for subscription masks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    ConfigChanged,
    ThemeChanged,
    Suspended,
    ClipboardItem,
    ClipboardRemoved,
    Notification,
    FileDropped,
    DragHover,
    Battery,
    FocusChanged,
    MediaChanged,
    Calendar,
    StoreLoaded,
    Privacy,
    Downloads,
    DownloadDone,
    Stats,
    Control,
    PhoneLink,
    Inbound,
}

impl Kind {
    pub const ALL: [Kind; 20] = [
        Kind::ConfigChanged,
        Kind::ThemeChanged,
        Kind::Suspended,
        Kind::ClipboardItem,
        Kind::ClipboardRemoved,
        Kind::Notification,
        Kind::FileDropped,
        Kind::DragHover,
        Kind::Battery,
        Kind::FocusChanged,
        Kind::MediaChanged,
        Kind::Calendar,
        Kind::StoreLoaded,
        Kind::Privacy,
        Kind::Downloads,
        Kind::DownloadDone,
        Kind::Stats,
        Kind::Control,
        Kind::PhoneLink,
        Kind::Inbound,
    ];
}

impl EventKind {
    pub fn kind(&self) -> Kind {
        match self {
            EventKind::ConfigChanged => Kind::ConfigChanged,
            EventKind::ThemeChanged => Kind::ThemeChanged,
            EventKind::Suspended(_) => Kind::Suspended,
            EventKind::ClipboardItem(_) => Kind::ClipboardItem,
            EventKind::ClipboardRemoved(_) => Kind::ClipboardRemoved,
            EventKind::Notification(_) => Kind::Notification,
            EventKind::FileDropped(_) => Kind::FileDropped,
            EventKind::DragHover(_) => Kind::DragHover,
            EventKind::Battery(_) => Kind::Battery,
            EventKind::FocusChanged(_) => Kind::FocusChanged,
            EventKind::MediaChanged(_) => Kind::MediaChanged,
            EventKind::Calendar(_) => Kind::Calendar,
            EventKind::StoreLoaded(_) => Kind::StoreLoaded,
            EventKind::Privacy(_) => Kind::Privacy,
            EventKind::Downloads(_) => Kind::Downloads,
            EventKind::DownloadDone(_) => Kind::DownloadDone,
            EventKind::Stats(_) => Kind::Stats,
            EventKind::Control(_) => Kind::Control,
            EventKind::PhoneLink(_) => Kind::PhoneLink,
            EventKind::Inbound(_) => Kind::Inbound,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub source: Source,
    pub at: Instant,
    pub kind: EventKind,
}

impl Event {
    pub fn new(source: Source, kind: EventKind) -> Self {
        Self {
            source,
            at: Instant::now(),
            kind,
        }
    }
}

/// A set of event kinds a module wants to hear about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EventMask(u32);

impl EventMask {
    pub const NONE: EventMask = EventMask(0);

    pub fn of(kinds: &[Kind]) -> EventMask {
        EventMask(kinds.iter().fold(0, |m, k| m | (1 << *k as u8)))
    }

    pub fn contains(self, k: Kind) -> bool {
        self.0 & (1 << k as u8) != 0
    }

    pub fn union(self, o: EventMask) -> EventMask {
        EventMask(self.0 | o.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<EventKind> {
        vec![
            EventKind::ConfigChanged,
            EventKind::ThemeChanged,
            EventKind::Suspended(true),
            EventKind::ClipboardItem(ClipboardItem {
                id: 1,
                kind: ClipKind::Text,
                preview: "x".into(),
                thumb: 0,
                pinned: false,
            }),
            EventKind::ClipboardRemoved(1),
            EventKind::Notification(Notification {
                id: 1,
                app: "a".into(),
                title: "t".into(),
                body: "b".into(),
                icon: 0,
                fresh: true,
                ago_secs: 0,
                quiet: false,
            }),
            EventKind::FileDropped(vec![]),
            EventKind::DragHover(true),
            EventKind::Battery(BatteryInfo {
                percent: 50,
                charging: false,
            }),
            EventKind::FocusChanged(FocusInfo {
                name: "Work".into(),
                active: true,
            }),
            EventKind::MediaChanged(Arc::new(MediaSnapshot::default())),
            EventKind::Calendar(Arc::new(CalendarData::default())),
            EventKind::StoreLoaded(StoreItem {
                key: "k".into(),
                data: None,
            }),
            EventKind::Privacy(Arc::new(PrivacyState::default())),
            EventKind::Downloads(Arc::new(Vec::new())),
            EventKind::DownloadDone(DownloadDone {
                name: "a.zip".into(),
                path: "C:\\a.zip".into(),
                bytes: 1,
            }),
            EventKind::PhoneLink(Arc::new(PhoneLink::default())),
            EventKind::Inbound(Inbound::Text("hello".into())),
            EventKind::Control(Arc::new(ControlState {
                volume: None,
                brightness: None,
                wifi: Radio::Unavailable,
                bluetooth: Radio::Unavailable,
                dnd: None,
            })),
            EventKind::Stats(Arc::new(StatsSnapshot {
                cpu: None,
                mem_used: 0,
                mem_total: 0,
                gpu: None,
                net: None,
                power: None,
            })),
        ]
    }

    #[test]
    fn every_event_maps_to_a_distinct_kind() {
        let kinds: Vec<Kind> = samples().iter().map(EventKind::kind).collect();
        assert_eq!(kinds.len(), Kind::ALL.len());
        for k in Kind::ALL {
            assert_eq!(kinds.iter().filter(|x| **x == k).count(), 1, "{k:?}");
        }
    }

    #[test]
    fn masks_select_exactly_what_was_asked_for() {
        let m = EventMask::of(&[Kind::Battery, Kind::Notification]);
        assert!(m.contains(Kind::Battery) && m.contains(Kind::Notification));
        assert!(!m.contains(Kind::MediaChanged));
        assert!(!EventMask::NONE.contains(Kind::Battery));
        let u = m.union(EventMask::of(&[Kind::MediaChanged]));
        assert!(u.contains(Kind::MediaChanged) && u.contains(Kind::Battery));
    }

    #[test]
    fn local_and_phone_events_differ_only_by_source() {
        let kind = EventKind::Battery(BatteryInfo {
            percent: 80,
            charging: true,
        });
        let a = Event::new(Source::Local, kind.clone());
        let b = Event::new(Source::Phone, kind);
        assert_ne!(a.source, b.source);
        assert_eq!(
            a.kind, b.kind,
            "same payload type, rendered by the same code"
        );
    }
}
