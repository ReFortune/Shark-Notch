//! The iPhone link's platform side: a small HTTP server for the local network.
//!
//! It is one thread asleep in `accept` on a TCP port, plus one short-lived thread per request while
//! one is being served (at most [`MAX_CONNECTIONS`]). Nothing runs on a timer: with no phone
//! talking, the cost is a listening socket and a sleeping thread. Everything that decides what is
//! accepted (the request parser, the token check, the lockout, who may connect, what each path
//! means) lives in `notch_core::phone`, where it is tested on any OS and fuzzed; this file moves
//! bytes and touches the file system.
//!
//! The order of the checks is deliberate:
//!
//! 1. the peer: a private address, and by default on one of this PC's own networks. Anything else is
//!    dropped before a byte is read;
//! 2. the request head, read with a size limit and a deadline;
//! 3. the lockout and the token. An unauthenticated peer learns nothing else, not even which paths
//!    exist;
//! 4. the path, and whether the PC has that feature switched on;
//! 5. the declared body length against the limit, before any body byte is read;
//! 6. the body: JSON is read into memory (64 KiB at most), a file is streamed to disk.
//!
//! Received files are written under `phone-inbox\<time>-<random>\<name>` in the app's data folder,
//! never anywhere the sender picks, carry the "came from the internet" mark (`Zone.Identifier`) so
//! SmartScreen and Office treat them with suspicion, and are never opened or run by this app. The
//! shelf then holds a reference to them like to any other file.
//!
//! What it is not: a general web server. HTTP/1.1 subset, one request per connection, `Content-Length`
//! bodies only, IPv4 only, plain HTTP (see `docs/IPHONE_SHORTCUTS.md` for what that means).

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notch_core::bus::BusSender;
use notch_core::config::{Config, PhoneCfg};
use notch_core::events::{
    BatteryInfo, EventKind, FocusInfo, Inbound, Notification, PhoneLink, Source,
};
use notch_core::phone::{self as proto, Action, Head, HttpError, LocalNet, LoginGuard, Route};
use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::win::paths;

/// At most this many requests are served at once; more connections are closed unanswered.
const MAX_CONNECTIONS: usize = 8;
/// A request head must arrive within this long.
const HEAD_TIME: Duration = Duration::from_secs(10);
/// A JSON request (head and body) must be complete within this long.
const JSON_TIME: Duration = Duration::from_secs(20);
/// A file may take this long, plus what [`MIN_RATE`] allows for its size.
const FILE_TIME: Duration = Duration::from_secs(60);
/// A file upload must average at least this many bytes per second (a stalled one is cut off).
const MIN_RATE: u64 = 256 * 1024;
/// One read on a socket waits at most this long.
const READ_WAIT: Duration = Duration::from_secs(10);
/// After an answer, request bytes still in flight are read away (this much, this long) so that
/// closing the socket does not reset the connection and take the answer with it.
const LINGER_BYTES: usize = 256 * 1024;
const LINGER_TIME: Duration = Duration::from_millis(800);
/// How long the list of this PC's networks is trusted, and how soon it may be re-read when a peer
/// is not on any known network (a phone that has just joined).
const NETS_TTL: Duration = Duration::from_secs(10);
const NETS_GAP: Duration = Duration::from_secs(2);
/// The state shown on the page is published at most this often while requests arrive.
const PUBLISH_GAP: Duration = Duration::from_secs(1);
/// The inbox may hold at least this much; more when single files are allowed to be larger.
const INBOX_MIN_CAP: u64 = 2 * 1024 * 1024 * 1024;

// ----- settings ------------------------------------------------------------------------------

/// Which features of the PC a request may use (`Route::needs`): a route whose module is switched
/// off is refused with a message saying so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    pub clipboard: bool,
    pub shelf: bool,
    pub notifications: bool,
}

impl Features {
    pub fn of(cfg: &Config) -> Features {
        Features {
            clipboard: cfg.module_active("clipboard"),
            shelf: cfg.module_active("shelf"),
            notifications: cfg.module_active("notifications"),
        }
    }

    fn allow(self, need: Option<&str>) -> Result<(), HttpError> {
        match need {
            Some("clipboard") if !self.clipboard => Err(HttpError::Disabled(
                "the clipboard module is switched off on the PC",
            )),
            Some("shelf") if !self.shelf => Err(HttpError::Disabled(
                "the file shelf is switched off on the PC",
            )),
            Some("notifications") if !self.notifications => Err(HttpError::Disabled(
                "notifications are switched off on the PC",
            )),
            _ => Ok(()),
        }
    }
}

/// The settings that may change while the listener runs.
#[derive(Clone, Copy, Debug)]
struct Settings {
    /// Only phones on one of the PC's own networks (not any private address).
    strict: bool,
    max_file_bytes: u64,
    keep_days: u32,
    features: Features,
}

impl Settings {
    fn of(cfg: &PhoneCfg, features: Features) -> Settings {
        Settings {
            strict: cfg.same_network_only,
            max_file_bytes: u64::from(cfg.max_file_mib) * 1024 * 1024,
            keep_days: cfg.keep_days,
            features,
        }
    }
}

// ----- state shared by the accept thread, the request threads and the UI thread ---------------

struct State {
    /// Empty until the listener thread has read or made it (then nothing is accepted).
    token: String,
    settings: Settings,
    guard: LoginGuard,
    nets: Vec<LocalNet>,
    nets_at: Option<Instant>,
    port: u16,
    last: Option<(i64, Arc<str>)>,
    accepted: u32,
    refused: u32,
    /// Counter for the ids of notifications from the phone.
    notes: u64,
    /// What was last sent to the page, and when.
    sent: Option<PhoneLink>,
    sent_at: Option<Instant>,
}

struct Shared {
    bus: BusSender,
    inbox: PathBuf,
    token_path: PathBuf,
    /// The lockout's clock (seconds since the listener started).
    t0: Instant,
    /// Requests being served.
    active: AtomicUsize,
    quit: AtomicBool,
    /// A publish is already waiting for the end of the gap (one is enough, however many requests
    /// arrive meanwhile).
    trailing: AtomicBool,
    state: Mutex<State>,
}

/// The listener's inbox slot a request holds while it is served.
struct Slot(Arc<Shared>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A request thread that panicked must not take the listener down with it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn settings(&self) -> Settings {
        self.lock().settings
    }

    fn link(st: &State) -> PhoneLink {
        PhoneLink {
            addrs: proto::display_addrs(&st.nets, st.port)
                .into_iter()
                .map(Arc::from)
                .collect(),
            port: st.port,
            error: None,
            last: st.last.clone(),
            accepted: st.accepted,
            refused: st.refused,
        }
    }

    /// Tell the page how things stand: always (`force`), or only when something changed since the
    /// last time.
    fn publish(&self, force: bool) {
        let link = {
            let mut st = self.lock();
            let link = Shared::link(&st);
            if !force && st.sent.as_ref() == Some(&link) {
                return;
            }
            st.sent = Some(link.clone());
            st.sent_at = Some(Instant::now());
            link
        };
        self.bus
            .send(Source::Local, EventKind::PhoneLink(Arc::new(link)));
    }

    /// Tell the page soon, but never more than once per [`PUBLISH_GAP`]: a burst of requests is one
    /// redraw, and a stranger knocking cannot make the UI work more than that. What the gap holds
    /// back is sent when it is over (one trailing publish, from a thread that lives that long).
    fn publish_paced(self: &Arc<Shared>) {
        let wait = self
            .lock()
            .sent_at
            .map_or(Duration::ZERO, |t| PUBLISH_GAP.saturating_sub(t.elapsed()));
        if wait.is_zero() {
            self.publish(false);
            return;
        }
        if self.trailing.swap(true, Ordering::AcqRel) {
            return;
        }
        let sh = self.clone();
        let spawned = std::thread::Builder::new()
            .name("phone-publish".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                std::thread::sleep(wait);
                sh.trailing.store(false, Ordering::Release);
                if !sh.quit.load(Ordering::Acquire) {
                    sh.publish(false);
                }
            });
        if spawned.is_err() {
            self.trailing.store(false, Ordering::Release);
        }
    }

    /// Look at this PC's networks again (a system call: never with the lock held).
    fn refresh_nets(&self) {
        let nets = local_networks();
        let mut st = self.lock();
        st.nets = nets;
        st.nets_at = Some(Instant::now());
    }

    /// May a peer at `ip` talk to the PC? See [`proto::peer_allowed`] for the rule; this adds the
    /// list of the PC's networks, kept fresh without letting a flood of connections hammer the
    /// system call that reads it.
    fn peer_allowed(&self, ip: Ipv4Addr) -> bool {
        if ip.is_loopback() {
            return true;
        }
        if !proto::is_private_v4(ip) {
            return false;
        }
        let (strict, known, age) = {
            let st = self.lock();
            (
                st.settings.strict,
                proto::peer_allowed(ip, &proto::net_pairs(&st.nets), true),
                st.nets_at.map(|t| t.elapsed()),
            )
        };
        if !strict {
            return true;
        }
        if known && age.is_some_and(|a| a < NETS_TTL) {
            return true;
        }
        if age.is_some_and(|a| a < NETS_GAP) {
            // Just looked: whatever that showed stands.
            return known;
        }
        self.refresh_nets();
        let st = self.lock();
        proto::peer_allowed(ip, &proto::net_pairs(&st.nets), true)
    }

    fn authenticate(&self, peer: Ipv4Addr, head: &Head) -> Auth {
        let mut st = self.lock();
        let now = self.t0.elapsed().as_secs_f64();
        if let Some(left) = st.guard.locked(peer, now) {
            return Auth::Locked(left.ceil().max(1.0) as u64);
        }
        if proto::token_ok(head, &st.token) {
            st.guard.success(peer);
            Auth::Ok
        } else {
            if st.guard.failure(peer, now) {
                crate::warn!(
                    "phone: too many wrong tokens from {peer}; it is locked out for a while"
                );
            }
            Auth::Wrong
        }
    }

    fn count_accepted(self: &Arc<Shared>, what: &'static str) {
        {
            let mut st = self.lock();
            st.accepted = st.accepted.saturating_add(1);
            st.last = Some((unix_now(), what.into()));
        }
        self.publish_paced();
    }

    fn count_refused(self: &Arc<Shared>) {
        {
            let mut st = self.lock();
            st.refused = st.refused.saturating_add(1);
        }
        self.publish_paced();
    }

    /// Turn what the phone asked for into an event with the phone as its source. (The listener
    /// never calls another service: the app puts text on the clipboard history and files on the
    /// shelf when it sees the `Inbound` event.)
    fn apply(&self, action: Action) -> Done {
        let (what, kind) = match action {
            Action::Clipboard(text) => {
                ("clipboard", EventKind::Inbound(Inbound::Text(text.into())))
            }
            Action::Notify { app, title, body } => {
                let id = {
                    let mut st = self.lock();
                    st.notes += 1;
                    (1u64 << 32) | (st.notes & 0xFFFF_FFFF)
                };
                let app = if app.is_empty() { "iPhone".into() } else { app };
                (
                    "notification",
                    EventKind::Notification(Notification {
                        id,
                        app: app.into(),
                        title: title.into(),
                        body: body.into(),
                        icon: 0,
                        fresh: true,
                        ago_secs: 0,
                        quiet: false,
                    }),
                )
            }
            Action::Battery { percent, charging } => (
                "battery",
                EventKind::Battery(BatteryInfo { percent, charging }),
            ),
            Action::Focus { name, active } => (
                "Focus",
                EventKind::FocusChanged(FocusInfo {
                    name: name.into(),
                    active,
                }),
            ),
        };
        self.bus.send(Source::Phone, kind);
        Done::new(what, proto::ok_body(&[]))
    }

    /// Is there room in the inbox for `len` more bytes?
    fn check_room(&self, len: u64) -> Result<(), HttpError> {
        let cap = self
            .settings()
            .max_file_bytes
            .saturating_mul(4)
            .max(INBOX_MIN_CAP);
        if inbox_size(&self.inbox, cap).saturating_add(len) > cap {
            Err(HttpError::Storage(
                "the inbox on the PC is full; delete old files from it",
            ))
        } else {
            Ok(())
        }
    }
}

enum Auth {
    Ok,
    Wrong,
    /// Locked out; seconds left.
    Locked(u64),
}

/// A request refused, and what the phone is told.
struct Refusal {
    err: HttpError,
    retry_after: Option<u64>,
}

impl From<HttpError> for Refusal {
    fn from(err: HttpError) -> Refusal {
        Refusal {
            err,
            retry_after: None,
        }
    }
}

/// A request served.
struct Done {
    /// What the page says came in ("clipboard", "file", ...).
    what: &'static str,
    body: String,
    /// Old files in the inbox are cleared out after this one (not while the phone waits).
    sweep: bool,
}

impl Done {
    fn new(what: &'static str, body: String) -> Done {
        Done {
            what,
            body,
            sweep: false,
        }
    }
}

// ----- the service ---------------------------------------------------------------------------

pub struct PhoneService {
    shared: Arc<Shared>,
    /// The configured `port` this was started with (0 = pick one).
    setting_port: u16,
    /// Where to connect to wake the accept thread when it is time to stop.
    wake: SocketAddr,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
    refreshing: Arc<AtomicBool>,
}

impl PhoneService {
    /// Open the port and start listening. If the port cannot be opened the page is told why (a
    /// `PhoneLink` with an error) and there is no service.
    pub fn start(cfg: &PhoneCfg, features: Features, bus: BusSender) -> Option<PhoneService> {
        let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, cfg.port)) {
            Ok(l) => l,
            Err(e) => {
                let why = bind_error(&e, cfg.port);
                crate::warn!("phone: {why}");
                bus.send(
                    Source::Local,
                    EventKind::PhoneLink(Arc::new(PhoneLink {
                        error: Some(why.into()),
                        ..PhoneLink::default()
                    })),
                );
                return None;
            }
        };
        let port = listener.local_addr().map_or(cfg.port, |a| a.port());
        let data = paths::data_dir();
        let shared = Arc::new(Shared {
            bus,
            inbox: data.join("phone-inbox"),
            token_path: data.join("phone-token"),
            t0: Instant::now(),
            active: AtomicUsize::new(0),
            quit: AtomicBool::new(false),
            trailing: AtomicBool::new(false),
            state: Mutex::new(State {
                token: String::new(),
                settings: Settings::of(cfg, features),
                guard: LoginGuard::new(),
                nets: Vec::new(),
                nets_at: None,
                port,
                last: None,
                accepted: 0,
                refused: 0,
                notes: 0,
                sent: None,
                sent_at: None,
            }),
        });
        let done = Arc::new(AtomicBool::new(false));
        let (sh, d2) = (shared.clone(), done.clone());
        let thread = std::thread::Builder::new()
            .name("phone".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(listener, &sh);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the iPhone listener: {e}"))
            .ok()?;
        crate::info!("phone: listening on port {port}");
        Some(PhoneService {
            shared,
            setting_port: cfg.port,
            wake: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            thread: Some(thread),
            done,
            refreshing: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The configured port this was started with.
    pub fn setting_port(&self) -> u16 {
        self.setting_port
    }

    /// The port actually listened on (differs from the setting when that is 0).
    pub fn port(&self) -> u16 {
        self.shared.lock().port
    }

    /// Has the listener thread ended (the system kept refusing connections)?
    pub fn is_dead(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// The pairing token, once the listener has read or made it.
    pub fn token(&self) -> Option<String> {
        let t = self.shared.lock().token.clone();
        (!t.is_empty()).then_some(t)
    }

    /// Apply changed settings (everything except the port) without restarting.
    pub fn configure(&self, cfg: &PhoneCfg, features: Features) {
        self.shared.lock().settings = Settings::of(cfg, features);
    }

    /// Make a new token. The old one stops working at once, as does every lockout.
    pub fn new_token(&self) -> bool {
        let Some(token) = make_token() else {
            crate::warn!("phone: the system could not provide random bytes for a new token");
            return false;
        };
        {
            let mut st = self.shared.lock();
            st.token = token.clone();
            st.guard = LoginGuard::new();
        }
        // The file is written off the UI thread.
        let path = self.shared.token_path.clone();
        let _ = std::thread::Builder::new()
            .name("phone-token".into())
            .stack_size(128 * 1024)
            .spawn(move || save_token(&path, &token));
        true
    }

    /// Look at the PC's addresses again and tell the page if they changed (it asks every few
    /// seconds while it is on screen). The system call runs on a short-lived thread.
    pub fn refresh(&self) {
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let (sh, flag) = (self.shared.clone(), self.refreshing.clone());
        let spawned = std::thread::Builder::new()
            .name("phone-refresh".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                sh.refresh_nets();
                sh.publish(false);
                flag.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.refreshing.store(false, Ordering::Release);
        }
    }

    pub fn stop(mut self) {
        self.shared.quit.store(true, Ordering::Release);
        // `accept` has no timeout: connecting to ourselves is what wakes it.
        let _ = TcpStream::connect_timeout(&self.wake, Duration::from_millis(250));
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(500);
            while !self.done.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.done.load(Ordering::Acquire) {
                let _ = h.join();
            }
        }
    }
}

fn bind_error(e: &io::Error, port: u16) -> String {
    match e.kind() {
        io::ErrorKind::AddrInUse => {
            format!("Port {port} is already used by another program. Change `port` under [phone].")
        }
        io::ErrorKind::PermissionDenied => {
            format!("Windows would not let this app open port {port} ({e}).")
        }
        _ => format!("Cannot listen on port {port}: {e}"),
    }
}

// ----- the accept thread ---------------------------------------------------------------------

fn run(listener: TcpListener, sh: &Arc<Shared>) {
    // Everything slow happens here, not on the UI thread: reading the token file, the system's
    // list of networks, clearing out old files.
    let token = load_token(&sh.token_path);
    sh.lock().token = token.clone().unwrap_or_default();
    if token.is_none() {
        crate::warn!("phone: no token could be made; every request will be refused");
    }
    sh.refresh_nets();
    sh.publish(true);
    sweep(&sh.inbox, unix_now(), sh.settings().keep_days);

    let mut errors = 0u32;
    loop {
        let (stream, addr) = match listener.accept() {
            Ok(x) => {
                errors = 0;
                x
            }
            Err(e) => {
                if sh.quit.load(Ordering::Acquire) {
                    break;
                }
                errors += 1;
                if errors > 20 {
                    crate::warn!(
                        "phone: the system kept refusing connections ({e}); the listener stopped"
                    );
                    sh.bus.send(
                        Source::Local,
                        EventKind::PhoneLink(Arc::new(PhoneLink {
                            error: Some("The listener stopped after repeated errors.".into()),
                            ..PhoneLink::default()
                        })),
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(50 * u64::from(errors.min(10))));
                continue;
            }
        };
        if sh.quit.load(Ordering::Acquire) {
            break;
        }
        let SocketAddr::V4(peer) = addr else {
            continue;
        };
        let ip = *peer.ip();
        // Not on a network this PC is on (or not private at all): close without a word.
        if !sh.peer_allowed(ip) {
            sh.count_refused();
            continue;
        }
        if sh.active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            sh.active.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let slot = Slot(sh.clone());
        let spawned = std::thread::Builder::new()
            .name("phone-request".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let sh = slot.0.clone();
                serve(&sh, stream, ip);
                drop(slot);
            });
        if let Err(e) = spawned {
            // The closure (and with it the slot) was dropped by the failed spawn.
            crate::warn!("phone: cannot serve a request: {e}");
        }
    }
}

// ----- one request ---------------------------------------------------------------------------

fn serve(sh: &Arc<Shared>, mut s: TcpStream, peer: Ipv4Addr) {
    let _ = s.set_nodelay(true);
    let started = Instant::now();
    let (head, early) = match read_head(sh, &mut s, started + HEAD_TIME) {
        Ok(Some(x)) => x,
        // The phone went away, or never finished: nothing to answer.
        Ok(None) => return,
        Err(e) => {
            sh.count_refused();
            finish(&mut s, &proto::error_response(e, None));
            return;
        }
    };
    match handle(sh, &mut s, peer, &head, early, started) {
        Ok(done) => {
            sh.count_accepted(done.what);
            crate::debug!("phone: {} from {peer}", done.what);
            finish(&mut s, &proto::response((200, "OK"), &done.body, &[]));
            if done.sweep {
                sweep(&sh.inbox, unix_now(), sh.settings().keep_days);
            }
        }
        Err(r) => {
            sh.count_refused();
            crate::debug!("phone: refused a request from {peer}: {}", r.err.message());
            finish(&mut s, &proto::error_response(r.err, r.retry_after));
        }
    }
}

fn handle(
    sh: &Shared,
    s: &mut TcpStream,
    peer: Ipv4Addr,
    head: &Head,
    early: Vec<u8>,
    started: Instant,
) -> Result<Done, Refusal> {
    match sh.authenticate(peer, head) {
        Auth::Ok => {}
        Auth::Wrong => return Err(HttpError::Unauthorized.into()),
        Auth::Locked(secs) => {
            return Err(Refusal {
                err: HttpError::Locked,
                retry_after: Some(secs),
            });
        }
    }
    let route = proto::route(head)?;
    let settings = sh.settings();
    settings.features.allow(route.needs())?;
    let len = proto::body_length(route, head, settings.max_file_bytes)?;
    // A client that asked "may I send the body?" is answered before it sends it.
    if head.expect_continue && len > 0 {
        let _ = s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    match route {
        Route::Ping => Ok(Done::new(
            "connection test",
            proto::ok_body(&[
                ("name", "Shark Notch"),
                ("version", env!("CARGO_PKG_VERSION")),
            ]),
        )),
        Route::File => receive_file(sh, s, head, early, len, started),
        Route::Clipboard | Route::Notify | Route::Battery | Route::Focus => {
            let body = read_body(sh, s, early, len, started + JSON_TIME)?;
            let action = proto::parse_body(route, head.header("content-type"), &body)?;
            Ok(sh.apply(action))
        }
    }
}

/// One read, bounded by the deadline. `None`: it passed, the socket failed or timed out, or the
/// service is stopping.
fn read_some(
    s: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
    quit: &AtomicBool,
) -> Option<usize> {
    if quit.load(Ordering::Acquire) {
        return None;
    }
    let left = deadline.checked_duration_since(Instant::now())?;
    if left.is_zero() {
        return None;
    }
    s.set_read_timeout(Some(left.min(READ_WAIT))).ok()?;
    loop {
        match s.read(buf) {
            Ok(n) => return Some(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

/// Read until the request head is complete. Returns it with whatever body bytes came along.
fn read_head(
    sh: &Shared,
    s: &mut TcpStream,
    deadline: Instant,
) -> Result<Option<(Head, Vec<u8>)>, HttpError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(head) = proto::parse_head(&buf)? {
            let rest = buf.split_off(head.head_len);
            return Ok(Some((head, rest)));
        }
        match read_some(s, &mut chunk, deadline, &sh.quit) {
            Some(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => return Ok(None),
        }
    }
}

/// The body of a JSON request (`len` was checked against the limit already).
fn read_body(
    sh: &Shared,
    s: &mut TcpStream,
    mut body: Vec<u8>,
    len: u64,
    deadline: Instant,
) -> Result<Vec<u8>, Refusal> {
    let len = len as usize;
    body.truncate(len);
    let mut chunk = [0u8; 4096];
    while body.len() < len {
        let want = chunk.len().min(len - body.len());
        match read_some(s, &mut chunk[..want], deadline, &sh.quit) {
            Some(n) if n > 0 => body.extend_from_slice(&chunk[..n]),
            _ => return Err(HttpError::BadRequest("the body did not arrive in full").into()),
        }
    }
    Ok(body)
}

/// Stream a file to `phone-inbox\<time>-<random>\<name>`. A file that does not arrive in full is
/// deleted: the shelf only ever hears about whole files.
fn receive_file(
    sh: &Shared,
    s: &mut TcpStream,
    head: &Head,
    early: Vec<u8>,
    len: u64,
    started: Instant,
) -> Result<Done, Refusal> {
    if len == 0 {
        return Err(HttpError::BadRequest("the file is empty").into());
    }
    let now = unix_now();
    let name = proto::file_name_for(head, head.header("content-type"), &local_stamp())?;
    sh.check_room(len)?;
    let dir = sh.inbox.join(proto::inbox_dir_name(now, random_u32()));
    let storage = HttpError::Storage("the PC could not save the file");
    std::fs::create_dir_all(&dir).map_err(|e| {
        crate::warn!("phone: cannot create {}: {e}", dir.display());
        storage
    })?;
    let path = dir.join(&name);
    let deadline = started + FILE_TIME + Duration::from_secs(len / MIN_RATE);
    if let Err(r) = stream_to_file(sh, s, &path, &early, len, deadline) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(r);
    }
    mark_of_the_web(&path);
    sh.bus.send(
        Source::Phone,
        EventKind::Inbound(Inbound::File(path.to_string_lossy().into_owned().into())),
    );
    Ok(Done {
        what: "file",
        body: proto::ok_body(&[("saved", &name)]),
        sweep: true,
    })
}

fn stream_to_file(
    sh: &Shared,
    s: &mut TcpStream,
    path: &Path,
    early: &[u8],
    len: u64,
    deadline: Instant,
) -> Result<(), Refusal> {
    let storage = |e: io::Error| {
        crate::warn!("phone: cannot write {}: {e}", path.display());
        Refusal::from(HttpError::Storage("the PC could not save the file"))
    };
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(storage)?;
    let first = early.len().min(usize::try_from(len).unwrap_or(usize::MAX));
    f.write_all(&early[..first]).map_err(storage)?;
    let mut left = len - first as u64;
    let mut buf = vec![0u8; 64 * 1024];
    while left > 0 {
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        match read_some(s, &mut buf[..want], deadline, &sh.quit) {
            Some(n) if n > 0 => {
                f.write_all(&buf[..n]).map_err(storage)?;
                left -= n as u64;
            }
            _ => return Err(HttpError::BadRequest("the file did not arrive in full").into()),
        }
    }
    f.flush().map_err(storage)
}

/// Mark a received file as having come from the internet (an NTFS stream next to it). This is what
/// makes SmartScreen and Office's Protected View look at it before it can do anything.
fn mark_of_the_web(path: &Path) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    if let Err(e) = std::fs::write(PathBuf::from(stream), proto::ZONE_IDENTIFIER) {
        crate::warn!(
            "phone: could not mark {} as downloaded ({e})",
            path.display()
        );
    }
}

/// Send the answer, then read away what the phone is still sending so that closing the connection
/// does not reset it before the answer is read (a `401` to a large upload, for one).
fn finish(s: &mut TcpStream, answer: &[u8]) {
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    if s.write_all(answer).is_err() {
        return;
    }
    let _ = s.flush();
    let _ = s.shutdown(Shutdown::Write);
    let until = Instant::now() + LINGER_TIME;
    let mut sink = [0u8; 4096];
    let mut total = 0;
    while total < LINGER_BYTES {
        let Some(left) = until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            break;
        };
        if s.set_read_timeout(Some(left)).is_err() {
            break;
        }
        match s.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
}

// ----- the inbox -----------------------------------------------------------------------------

/// Delete received files older than `keep_days`. Only folders this app named are looked at.
fn sweep(inbox: &Path, now: i64, keep_days: u32) {
    let Ok(entries) = std::fs::read_dir(inbox) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if proto::inbox_expired(name, now, keep_days)
            && e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink())
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Bytes in the inbox, counted until `stop_at` is passed.
fn inbox_size(inbox: &Path, stop_at: u64) -> u64 {
    let mut total = 0u64;
    let Ok(folders) = std::fs::read_dir(inbox) else {
        return 0;
    };
    for folder in folders.flatten().take(20_000) {
        let Ok(files) = std::fs::read_dir(folder.path()) else {
            continue;
        };
        for f in files.flatten().take(1_000) {
            total = total.saturating_add(f.metadata().map_or(0, |m| m.len()));
            if total >= stop_at {
                return total;
            }
        }
    }
    total
}

// ----- the token and randomness --------------------------------------------------------------

fn random_bytes(buf: &mut [u8]) -> bool {
    unsafe { BCryptGenRandom(None, buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.is_ok()
}

fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    if random_bytes(&mut b) {
        u32::from_le_bytes(b)
    } else {
        // Only to tell folders apart: the time will do if the system has no random bytes.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    }
}

fn make_token() -> Option<String> {
    let mut b = [0u8; proto::TOKEN_LEN];
    random_bytes(&mut b).then(|| proto::token_from_bytes(&b))
}

/// The saved token, or a new one (saved). `None` only if the system gave no random bytes.
fn load_token(path: &Path) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let t = text.trim();
        if proto::token_valid(t) {
            return Some(t.to_string());
        }
        crate::warn!("phone: the token file is damaged; making a new token");
    }
    let t = make_token()?;
    save_token(path, &t);
    Some(t)
}

fn save_token(path: &Path, token: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, token) {
        crate::warn!("phone: cannot save the token ({e}); it works until the app is restarted");
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// `20251006-141530` in local time: the default name of a file the phone sent without one.
fn local_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

// ----- this PC's networks --------------------------------------------------------------------

/// The IPv4 addresses of this PC on networks that are up (not loopback or tunnels).
fn local_networks() -> Vec<LocalNet> {
    // Gateways are only filled in when asked for: they tell the home network from a virtual switch.
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut len: u32 = 15 * 1024;
    for _ in 0..4 {
        // 8-byte words: the system's structures want that alignment.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let rc = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET.0),
                flags,
                None,
                Some(buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &raw mut len,
            )
        };
        if rc == ERROR_BUFFER_OVERFLOW.0 {
            // `len` now says how much room the list needs.
            continue;
        }
        if rc != 0 {
            return Vec::new();
        }
        // SAFETY: the system filled `buf` with a valid list, and `buf` outlives the walk.
        return unsafe { adapters(buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()) };
    }
    Vec::new()
}

/// Read the list `GetAdaptersAddresses` filled in.
///
/// # Safety
/// `first` must point to a list that call produced, valid for the whole call of this function.
unsafe fn adapters(first: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<LocalNet> {
    let mut out = Vec::new();
    let mut adapter = first;
    while !adapter.is_null() {
        let a = unsafe { &*adapter };
        if a.OperStatus == IfOperStatusUp
            && a.IfType != IF_TYPE_SOFTWARE_LOOPBACK
            && a.IfType != IF_TYPE_TUNNEL
        {
            let gateway = !a.FirstGatewayAddress.is_null();
            let mut unicast = a.FirstUnicastAddress;
            while !unicast.is_null() {
                let u = unsafe { &*unicast };
                let sa = u.Address.lpSockaddr;
                if !sa.is_null()
                    && unsafe { (*sa).sa_family } == AF_INET
                    && usize::try_from(u.Address.iSockaddrLength)
                        .is_ok_and(|n| n >= size_of::<SOCKADDR_IN>())
                {
                    let sin = unsafe { &*sa.cast::<SOCKADDR_IN>() };
                    // Stored in network byte order.
                    let ip = Ipv4Addr::from(u32::from_be(unsafe { sin.sin_addr.S_un.S_addr }));
                    out.push(LocalNet {
                        ip,
                        prefix: u.OnLinkPrefixLength.min(32),
                        gateway,
                    });
                }
                unicast = u.Next;
            }
        }
        adapter = a.Next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_has_networks_or_none_and_never_crashes() {
        // A CI runner has at least a virtual adapter; a machine with none gives an empty list.
        // Either way the walk over the system's list must be sound.
        let nets = local_networks();
        for n in &nets {
            assert!(n.prefix <= 32, "{n:?}");
            assert!(!n.ip.is_unspecified(), "{n:?}");
        }
    }

    #[test]
    fn a_new_token_is_valid_and_different_each_time() {
        let a = make_token().expect("random bytes");
        let b = make_token().expect("random bytes");
        assert!(proto::token_valid(&a) && proto::token_valid(&b));
        assert_ne!(a, b);
    }

    #[test]
    fn the_token_file_is_kept_and_a_damaged_one_replaced() {
        let dir = std::env::temp_dir().join(format!("shark-notch-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("phone-token");
        let first = load_token(&path).expect("a token");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first, "saved");
        assert_eq!(load_token(&path).as_deref(), Some(first.as_str()), "kept");
        std::fs::write(&path, "not a token").unwrap();
        let second = load_token(&path).expect("a token");
        assert!(proto::token_valid(&second) && second != first);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_folders_are_cleared_and_foreign_ones_are_not() {
        let dir = std::env::temp_dir().join(format!("shark-notch-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = 1_800_000_000;
        let old = dir.join(proto::inbox_dir_name(now - 30 * 86_400, 1));
        let fresh = dir.join(proto::inbox_dir_name(now - 86_400, 2));
        let foreign = dir.join("my-own-folder");
        for d in [&old, &fresh, &foreign] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("a.txt"), b"hello").unwrap();
        }
        assert_eq!(inbox_size(&dir, u64::MAX), 15);
        assert_eq!(
            inbox_size(&dir, 6),
            10,
            "counting stops once the cap is passed"
        );
        sweep(&dir, now, 14);
        assert!(!old.exists(), "older than 14 days");
        assert!(fresh.exists(), "newer");
        assert!(foreign.exists(), "not ours");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
