//! Windows notifications (toasts) as a bus producer, through `UserNotificationListener`.
//!
//! **Package identity.** Windows only lets a process that has *package identity* read other apps'
//! notifications. A plain `shark-notch.exe` has none, so this service says so
//! (`NotificationAccess::NoIdentity`) and does nothing else; `docs/NOTIFICATIONS.md` and
//! `packaging/` describe how to give the exe an identity (a "sparse" MSIX package, no repackaging).
//!
//! With identity the worker asks for access, subscribes to `NotificationChanged` and then sleeps in
//! `recv`. Each change makes it read the whole list once and compare with what it announced before
//! (`notch_core::notifsync::Tracker`): the event only says *that* something changed, and reading
//! the list is the robust way to learn what. Nothing polls.
//!
//! Notification text is never written to the log. Everything runs on this one worker thread; the UI
//! thread only ever sees bus events.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notch_core::bus::BusSender;
use notch_core::config::NotificationsCfg;
use notch_core::events::{EventKind, Notification, NotificationAccess, Source};
use notch_core::fullscreen::UserNotifState;
use notch_core::image::{ImageCache, ImageData};
use notch_core::module::NotifCmd;
use notch_core::notifsync::{Seen, Tracker, age_secs, compose_text};
use windows::ApplicationModel::AppInfo;
use windows::Foundation::{Size, TypedEventHandler};
use windows::UI::Notifications::Management::{
    UserNotificationListener, UserNotificationListenerAccessStatus as Access,
};
use windows::UI::Notifications::{
    KnownNotificationBindings, NotificationKinds, UserNotification,
    UserNotificationChangedEventArgs,
};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::win::imaging;

/// App logos are decoded to this many pixels on a side (drawn at 30 DIPs; 2x leaves room for DPI).
const LOGO_EDGE: u32 = 64;
const MAX_LOGO_BYTES: u64 = 4 << 20;
/// Distinct apps whose logo is cached (each is at most 16 KiB of pixels).
const MAX_LOGOS: usize = 64;
/// A burst of change events (a toast being added raises several) becomes one read.
const COALESCE: Duration = Duration::from_millis(60);
/// Windows notification ids fit in 32 bits; phone ids (phase 11) set bit 32.
const WINDOWS_ID_LIMIT: u64 = 1 << 32;

enum Req {
    /// The platform reported a change in the notification list.
    Changed,
    Cmd(NotifCmd),
    Configure(NotificationsCfg),
    Quit,
}

pub struct NotificationsService {
    tx: Sender<Req>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

impl NotificationsService {
    pub fn start(
        cfg: NotificationsCfg,
        bus: BusSender,
        images: Arc<ImageCache>,
    ) -> Option<NotificationsService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let (tx2, done2) = (tx.clone(), done.clone());
        let thread = std::thread::Builder::new()
            .name("notifications".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(rx, tx2, bus, images, cfg);
                done2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the notifications worker: {e}"))
            .ok()?;
        Some(NotificationsService {
            tx,
            thread: Some(thread),
            done,
        })
    }

    pub fn command(&self, cmd: NotifCmd) {
        let _ = self.tx.send(Req::Cmd(cmd));
    }

    pub fn configure(&self, cfg: NotificationsCfg) {
        let _ = self.tx.send(Req::Configure(cfg));
    }

    /// Stop the worker. Waits briefly; a worker stuck in a system call (the consent prompt) is left
    /// to finish by itself rather than freezing the UI.
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

/// `ERROR_INSUFFICIENT_BUFFER` (a packaged process asked for its name with no buffer) and
/// `ERROR_SUCCESS` both mean "this process has package identity"; `APPMODEL_ERROR_NO_PACKAGE`
/// (15700) means it has none.
pub fn identity_from_code(code: u32) -> bool {
    matches!(code, 0 | 122)
}

fn has_package_identity() -> bool {
    let mut len = 0u32;
    let rc = unsafe { GetCurrentPackageFullName(&mut len, None) };
    identity_from_code(rc.0)
}

struct ComGuard(bool);

impl ComGuard {
    fn mta() -> ComGuard {
        use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
        ComGuard(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::WinRT::RoUninitialize() };
        }
    }
}

fn open_settings() {
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("ms-settings:privacy-notifications"),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn run(
    rx: Receiver<Req>,
    tx: Sender<Req>,
    bus: BusSender,
    images: Arc<ImageCache>,
    cfg: NotificationsCfg,
) {
    let _com = ComGuard::mta();
    if !has_package_identity() {
        crate::info!(
            "notifications: this process has no package identity, so Windows will not share its \
             notifications (see docs/NOTIFICATIONS.md)"
        );
        bus.send(
            Source::Local,
            EventKind::NotificationAccess(NotificationAccess::NoIdentity),
        );
        while let Ok(req) = rx.recv() {
            match req {
                Req::Quit => break,
                // Identity cannot appear while the process runs; say the same thing again so a
                // module created later still learns it.
                Req::Cmd(NotifCmd::Recheck) => bus.send(
                    Source::Local,
                    EventKind::NotificationAccess(NotificationAccess::NoIdentity),
                ),
                Req::Cmd(NotifCmd::OpenSettings) => open_settings(),
                _ => {}
            }
        }
        return;
    }
    let listener = match UserNotificationListener::Current() {
        Ok(l) => l,
        Err(e) => {
            crate::warn!("notifications: the listener is unavailable ({e})");
            bus.send(
                Source::Local,
                EventKind::NotificationAccess(NotificationAccess::Denied),
            );
            while let Ok(req) = rx.recv() {
                match req {
                    Req::Quit => break,
                    Req::Cmd(NotifCmd::OpenSettings) => open_settings(),
                    _ => {}
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
        listener,
        token: None,
        access: NotificationAccess::Unknown,
        tracker: Tracker::new(cfg.max_items as usize),
        cfg,
        logos: HashMap::new(),
        wake_at: None,
    };
    w.refresh_access();
    w.event_loop();
    w.shutdown();
}

struct Worker {
    rx: Receiver<Req>,
    tx: Sender<Req>,
    bus: BusSender,
    images: Arc<ImageCache>,
    listener: UserNotificationListener,
    token: Option<i64>,
    access: NotificationAccess,
    tracker: Tracker,
    cfg: NotificationsCfg,
    /// App id -> image id of its logo (0 = it has none / could not be read).
    logos: HashMap<String, u64>,
    wake_at: Option<Instant>,
}

impl Worker {
    fn publish_access(&mut self, a: NotificationAccess, force: bool) {
        if force || a != self.access {
            self.bus
                .send(Source::Local, EventKind::NotificationAccess(a));
        }
        self.access = a;
    }

    /// Ask Windows what it allows. Only an undecided state asks the user (the system consent
    /// prompt); a decision, either way, is just read.
    fn refresh_access(&mut self) {
        let status = match self.listener.GetAccessStatus() {
            Ok(s @ (Access::Allowed | Access::Denied)) => Ok(s),
            _ => self.listener.RequestAccessAsync().and_then(|op| op.join()),
        };
        let now = match status {
            Ok(Access::Allowed) => NotificationAccess::Granted,
            Ok(_) => NotificationAccess::Denied,
            Err(e) => {
                crate::warn!("notifications: cannot query access ({e})");
                NotificationAccess::Denied
            }
        };
        self.publish_access(now, true);
        match now {
            NotificationAccess::Granted => {
                if self.token.is_none() {
                    self.attach();
                }
                self.sync();
            }
            _ => {
                self.detach();
                self.tracker.reset();
            }
        }
    }

    fn attach(&mut self) {
        let tx = self.tx.clone();
        let handler =
            TypedEventHandler::<UserNotificationListener, UserNotificationChangedEventArgs>::new(
                move |_, _| {
                    let _ = tx.send(Req::Changed);
                    Ok(())
                },
            );
        match self.listener.NotificationChanged(&handler) {
            Ok(t) => self.token = Some(t),
            Err(e) => crate::warn!(
                "notifications: cannot subscribe to changes ({e}); the list refreshes when the page opens"
            ),
        }
    }

    fn detach(&mut self) {
        if let Some(t) = self.token.take() {
            let _ = self.listener.RemoveNotificationChanged(t);
        }
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
                Ok(Req::Changed) => {
                    self.wake_at.get_or_insert(Instant::now() + COALESCE);
                }
                Ok(Req::Configure(cfg)) => self.cfg = cfg,
                Ok(Req::Cmd(cmd)) => self.execute(cmd),
                Err(RecvTimeoutError::Timeout) => {
                    self.wake_at = None;
                    self.sync();
                }
            }
        }
    }

    fn shutdown(&mut self) {
        self.detach();
        for (_, id) in self.logos.drain() {
            if id != 0 {
                self.images.remove(notch_core::draw::ImageId(id));
            }
        }
    }

    fn execute(&mut self, cmd: NotifCmd) {
        match cmd {
            NotifCmd::OpenSettings => open_settings(),
            NotifCmd::Recheck => self.refresh_access(),
            NotifCmd::Dismiss(id) => {
                if self.cfg.dismiss_in_windows
                    && self.access == NotificationAccess::Granted
                    && id < WINDOWS_ID_LIMIT
                {
                    let _ = self.listener.RemoveNotification(id as u32);
                }
            }
            NotifCmd::ClearAll => {
                if self.cfg.dismiss_in_windows && self.access == NotificationAccess::Granted {
                    let _ = self.listener.ClearNotifications();
                }
            }
        }
    }

    /// Read the current list and publish what is new or gone.
    fn sync(&mut self) {
        if self.access != NotificationAccess::Granted {
            return;
        }
        let list = match self
            .listener
            .GetNotificationsAsync(NotificationKinds::Toast)
            .and_then(|op| op.join())
        {
            Ok(l) => l,
            Err(e) => {
                crate::warn!("notifications: cannot read the list ({e})");
                // Access may have been withdrawn in the meantime.
                if !matches!(self.listener.GetAccessStatus(), Ok(Access::Allowed)) {
                    self.publish_access(NotificationAccess::Denied, false);
                    self.detach();
                    self.tracker.reset();
                }
                return;
            }
        };
        let count = list.Size().unwrap_or(0);
        let mut seen = Vec::with_capacity(count as usize);
        let mut items: HashMap<u64, UserNotification> = HashMap::with_capacity(count as usize);
        for i in 0..count {
            let Ok(n) = list.GetAt(i) else { continue };
            let Ok(id) = n.Id() else { continue };
            let created = n.CreationTime().map_or(0, |t| t.UniversalTime);
            seen.push(Seen {
                id: u64::from(id),
                created,
            });
            items.insert(u64::from(id), n);
        }
        let up = self.tracker.update(&seen);
        if !up.added.is_empty() || !up.removed.is_empty() {
            crate::debug!(
                "notifications: {} new, {} gone, {} listed",
                up.added.len(),
                up.removed.len(),
                seen.len()
            );
        }
        for (id, fresh) in up.added {
            if let Some(n) = items.get(&id) {
                self.publish(id, n, fresh);
            }
        }
        for id in up.removed {
            self.bus
                .send(Source::Local, EventKind::NotificationRemoved(id));
        }
    }

    fn publish(&mut self, id: u64, n: &UserNotification, fresh: bool) {
        let (app, logo) = match n.AppInfo() {
            Ok(info) => (
                info.DisplayInfo()
                    .and_then(|d| d.DisplayName())
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                self.logo_for(&info),
            ),
            Err(_) => (String::new(), 0),
        };
        let lines = text_lines(n);
        let (title, body) = compose_text(&lines);
        let created = n.CreationTime().map_or(0, |t| t.UniversalTime);
        self.bus.send(
            Source::Local,
            EventKind::Notification(Notification {
                id,
                app: app.into(),
                title: title.into(),
                body: body.into(),
                icon: logo,
                fresh,
                ago_secs: age_secs(created, now_filetime_ticks()),
                // Only worth asking about when it would otherwise be announced.
                quiet: fresh && in_quiet_time(),
            }),
        );
    }

    /// The app's logo as an image-cache id (decoded once per app).
    fn logo_for(&mut self, info: &AppInfo) -> u64 {
        let key = info
            .AppUserModelId()
            .map(|s| s.to_string())
            .unwrap_or_default();
        if let Some(&id) = self.logos.get(&key) {
            return id;
        }
        let id = fetch_logo(info)
            .and_then(|img| self.images.put(img))
            .map_or(0, |i| i.0);
        if !key.is_empty() && self.logos.len() < MAX_LOGOS {
            self.logos.insert(key, id);
        } else if id != 0 {
            // Not cached by key: it would never be freed, so do not keep it at all.
            self.images.remove(notch_core::draw::ImageId(id));
            return 0;
        }
        id
    }
}

/// The text elements of a toast's generic binding (title first, then body lines).
fn text_lines(n: &UserNotification) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(binding) = n
        .Notification()
        .and_then(|x| x.Visual())
        .and_then(|v| KnownNotificationBindings::ToastGeneric().and_then(|b| v.GetBinding(&b)))
    else {
        return out;
    };
    let Ok(texts) = binding.GetTextElements() else {
        return out;
    };
    // A toast has a handful of lines; never walk an absurd list an app might supply.
    for i in 0..texts.Size().unwrap_or(0).min(8) {
        if let Ok(t) = texts.GetAt(i).and_then(|t| t.Text()) {
            out.push(t.to_string());
        }
    }
    out
}

fn fetch_logo(info: &AppInfo) -> Option<ImageData> {
    let display = info.DisplayInfo().ok()?;
    let stream = display
        .GetLogo(Size {
            Width: LOGO_EDGE as f32,
            Height: LOGO_EDGE as f32,
        })
        .ok()?
        .OpenReadAsync()
        .ok()?
        .join()
        .ok()?;
    let bytes = imaging::read_stream(&stream, MAX_LOGO_BYTES)?;
    imaging::decode(&bytes, LOGO_EDGE)
}

/// Is Windows in Focus / do-not-disturb ("quiet time")? Its own toasts go straight to the
/// notification centre then, so the notch must not pop a banner for them either.
fn in_quiet_time() -> bool {
    unsafe { SHQueryUserNotificationState() }
        .is_ok_and(|s| UserNotifState::from_raw(s.0) == UserNotifState::QuietTime)
}

/// Current time as Windows `DateTime` ticks (100 ns since 1601-01-01 UTC).
fn now_filetime_ticks() -> i64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() as i64 + 11_644_473_600) * 10_000_000 + i64::from(d.subsec_nanos() / 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notch_core::bus::{Bus, Waker};

    struct NoWake;
    impl Waker for NoWake {
        fn wake(&self) {}
    }

    #[test]
    fn package_identity_is_read_from_the_win32_status_code() {
        assert!(!identity_from_code(15_700), "APPMODEL_ERROR_NO_PACKAGE");
        assert!(
            identity_from_code(122),
            "ERROR_INSUFFICIENT_BUFFER: packaged"
        );
        assert!(identity_from_code(0));
        assert!(
            !identity_from_code(5),
            "anything unexpected is not identity"
        );
    }

    #[test]
    fn the_test_process_has_no_identity() {
        // `cargo test` runs a plain exe, exactly like an unregistered Shark Notch.
        assert!(!has_package_identity());
    }

    #[test]
    fn without_identity_the_service_says_so_and_stays_idle() {
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = NotificationsService::start(
            NotificationsCfg::default(),
            tx,
            Arc::new(ImageCache::default()),
        )
        .expect("the worker starts");
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while got.is_empty() && Instant::now() < until {
            bus.drain(&mut got);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            got.iter().map(|e| e.kind.clone()).collect::<Vec<_>>(),
            vec![EventKind::NotificationAccess(
                NotificationAccess::NoIdentity
            )]
        );
        // Asking again repeats the answer (a module created later still learns it).
        svc.command(NotifCmd::Recheck);
        let mut again = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while again.is_empty() && Instant::now() < until {
            bus.drain(&mut again);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(again.len(), 1);
        svc.stop();
    }

    #[test]
    fn filetime_now_is_in_the_right_epoch() {
        assert!(now_filetime_ticks() > 132_223_104_000_000_000);
    }
}
