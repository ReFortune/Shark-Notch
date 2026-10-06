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
    pub id: u64,
    pub app: Arc<str>,
    pub title: Arc<str>,
    pub body: Arc<str>,
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
}

impl Kind {
    pub const ALL: [Kind; 11] = [
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
