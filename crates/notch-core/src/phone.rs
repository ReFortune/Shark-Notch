//! The iPhone link, as far as it needs no sockets: a small HTTP/1.1 subset, the token check, the
//! lockout for wrong guesses, which peers may talk to the PC, and what each request means.
//!
//! The listener (in the Windows crate) only moves bytes: it reads until [`parse_head`] says the
//! request head is complete, asks [`route`] and [`token_ok`] what to do **before reading any body**,
//! and hands the body to [`parse_body`]. Everything that decides what is accepted is here, where it
//! is tested on any OS — including against garbage, because this is the one place that parses bytes
//! from the network.
//!
//! What is deliberately small: one request per connection, `Content-Length` bodies only (no
//! chunked uploads), GET and POST, a handful of fixed paths. Nothing here is a general web server.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use serde_json::Value;

use crate::notifsync::tidy;

/// The request head (request line and headers) may not be longer than this.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;
/// JSON requests may not be longer than this.
pub const MAX_JSON_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_TARGET: usize = 2048;

/// Wrong guesses from one address, and what happens to it after too many.
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW_SECS: f64 = 60.0;
const LOCKOUT_SECS: f64 = 120.0;
const GUARD_CAP: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    /// Parsed, but not one the link serves (answered with 405).
    Other,
}

/// A parsed request head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub method: Method,
    /// The path as sent (not decoded: only fixed literal paths are served).
    pub path: String,
    /// The query as decoded `name=value` pairs.
    pub query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
    /// The client waits for `100 Continue` before it sends the body.
    pub expect_continue: bool,
    /// Bytes the head took including the blank line; the body starts here.
    pub head_len: usize,
}

impl Head {
    /// A header's value; `name` is matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a request was refused, and what is said about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// Not HTTP the way this server reads it.
    Malformed,
    /// The head is longer than [`MAX_HEAD_BYTES`].
    HeadTooLarge,
    NotFound,
    MethodNotAllowed,
    Unauthorized,
    /// Too many wrong tokens from this address: wait.
    Locked,
    LengthRequired,
    PayloadTooLarge,
    UnsupportedMedia(&'static str),
    BadRequest(&'static str),
    /// The PC has this feature switched off.
    Disabled(&'static str),
    /// The PC could not keep what was sent (disk full, inbox full, folder not writable).
    Storage(&'static str),
}

impl HttpError {
    /// `(status code, reason phrase)`.
    pub fn status(self) -> (u16, &'static str) {
        match self {
            HttpError::Malformed | HttpError::BadRequest(_) => (400, "Bad Request"),
            HttpError::Unauthorized => (401, "Unauthorized"),
            HttpError::Disabled(_) => (403, "Forbidden"),
            HttpError::NotFound => (404, "Not Found"),
            HttpError::MethodNotAllowed => (405, "Method Not Allowed"),
            HttpError::LengthRequired => (411, "Length Required"),
            HttpError::PayloadTooLarge => (413, "Payload Too Large"),
            HttpError::UnsupportedMedia(_) => (415, "Unsupported Media Type"),
            HttpError::Locked => (429, "Too Many Requests"),
            HttpError::HeadTooLarge => (431, "Request Header Fields Too Large"),
            HttpError::Storage(_) => (507, "Insufficient Storage"),
        }
    }

    /// What the client is told. Never says anything about the token beyond "not accepted".
    pub fn message(self) -> &'static str {
        match self {
            HttpError::Malformed => "that is not a request this server understands",
            HttpError::HeadTooLarge => "the request headers are too long",
            HttpError::NotFound => "no such address",
            HttpError::MethodNotAllowed => "that address does not take this method",
            HttpError::Unauthorized => "the token was not accepted",
            HttpError::Locked => {
                "too many wrong tokens from this address; try again in a few minutes"
            }
            HttpError::LengthRequired => "send a Content-Length with the body",
            HttpError::PayloadTooLarge => "the body is too large",
            HttpError::UnsupportedMedia(m)
            | HttpError::BadRequest(m)
            | HttpError::Disabled(m)
            | HttpError::Storage(m) => m,
        }
    }
}

// ----- the request head ----------------------------------------------------------------------

fn find_blank_line(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Decode `%XX` escapes (and `+` as a space when `plus_is_space`); `None` if an escape is broken
/// or the result is not UTF-8.
pub fn percent_decode(s: &str, plus_is_space: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = b.get(i + 1..i + 3)?;
                let hi = (hex[0] as char).to_digit(16)?;
                let lo = (hex[1] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Read a request head from what has arrived so far.
///
/// * `Ok(None)`: not complete yet, read more;
/// * `Ok(Some(head))`: complete (`head.head_len` bytes of `buf` are the head);
/// * `Err(_)`: refuse the request.
pub fn parse_head(buf: &[u8]) -> Result<Option<Head>, HttpError> {
    let Some(end) = find_blank_line(buf) else {
        return if buf.len() >= MAX_HEAD_BYTES {
            Err(HttpError::HeadTooLarge)
        } else {
            Ok(None)
        };
    };
    if end + 4 > MAX_HEAD_BYTES {
        return Err(HttpError::HeadTooLarge);
    }
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| HttpError::Malformed)?;
    let mut lines = text.split("\r\n");

    // Request line: METHOD SP target SP HTTP/1.x
    let request_line = lines.next().ok_or(HttpError::Malformed)?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::Malformed);
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(HttpError::Malformed);
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpError::Malformed);
    }
    if !target.starts_with('/')
        || target.len() > MAX_TARGET
        || target.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'#')
    {
        return Err(HttpError::Malformed);
    }
    let (path, query_str) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let mut query = Vec::new();
    for pair in query_str.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = percent_decode(k, true).ok_or(HttpError::Malformed)?;
        let v = percent_decode(v, true).ok_or(HttpError::Malformed)?;
        query.push((k, v));
        if query.len() > 32 {
            return Err(HttpError::Malformed);
        }
    }

    // Headers.
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_length: Option<u64> = None;
    let mut expect_continue = false;
    for line in lines {
        if headers.len() >= MAX_HEADERS {
            return Err(HttpError::HeadTooLarge);
        }
        // A continuation line (obsolete folding) is how header smuggling starts: refuse it.
        if line.starts_with([' ', '\t']) {
            return Err(HttpError::Malformed);
        }
        let (name, value) = line.split_once(':').ok_or(HttpError::Malformed)?;
        if name.is_empty() || !name.bytes().all(is_token_char) {
            return Err(HttpError::Malformed);
        }
        let value = value.trim_matches([' ', '\t']);
        if value.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f) {
            return Err(HttpError::Malformed);
        }
        let lname = name.to_ascii_lowercase();
        match lname.as_str() {
            "content-length" => {
                if value.is_empty()
                    || value.len() > 18
                    || !value.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(HttpError::Malformed);
                }
                let n: u64 = value.parse().map_err(|_| HttpError::Malformed)?;
                // Two different lengths are a request-smuggling attempt.
                if content_length.is_some_and(|prev| prev != n) {
                    return Err(HttpError::Malformed);
                }
                content_length = Some(n);
            }
            "transfer-encoding" => {
                return Err(HttpError::BadRequest(
                    "chunked uploads are not supported: send a Content-Length",
                ));
            }
            "expect" => expect_continue = value.eq_ignore_ascii_case("100-continue"),
            _ => {}
        }
        headers.push((lname, value.to_string()));
    }

    Ok(Some(Head {
        method: match method {
            "GET" => Method::Get,
            "POST" => Method::Post,
            _ => Method::Other,
        },
        path: path.to_string(),
        query,
        headers,
        content_length,
        expect_continue,
        head_len: end + 4,
    }))
}

// ----- the token and the lockout -------------------------------------------------------------

/// Equal, without stopping at the first difference (so the time it takes says nothing about how
/// much of a guess was right).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u32;
    for i in 0..a.len().max(b.len()) {
        diff |= u32::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// The token from `Authorization: Bearer <token>`.
pub fn bearer_token(head: &Head) -> Option<&str> {
    let v = head.header("authorization")?;
    let (scheme, token) = v.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

/// Does the request carry the right token? An empty expected token never matches.
pub fn token_ok(head: &Head, expected: &str) -> bool {
    !expected.is_empty()
        && bearer_token(head).is_some_and(|t| ct_eq(t.as_bytes(), expected.as_bytes()))
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    fails: u32,
    first: f64,
    locked_until: f64,
}

/// Counts wrong tokens per address and locks an address out for a while after a few.
#[derive(Debug, Default)]
pub struct LoginGuard {
    map: HashMap<Ipv4Addr, Entry>,
}

impl LoginGuard {
    pub fn new() -> LoginGuard {
        LoginGuard::default()
    }

    /// `Some(seconds left)` while `ip` is locked out.
    pub fn locked(&self, ip: Ipv4Addr, now: f64) -> Option<f64> {
        let e = self.map.get(&ip)?;
        (e.locked_until > now).then_some(e.locked_until - now)
    }

    /// Record a wrong token. Returns whether this one locked the address.
    pub fn failure(&mut self, ip: Ipv4Addr, now: f64) -> bool {
        if self.map.len() >= GUARD_CAP && !self.map.contains_key(&ip) {
            self.prune(now);
        }
        let e = self.map.entry(ip).or_insert(Entry {
            fails: 0,
            first: now,
            locked_until: 0.0,
        });
        if now - e.first > FAILURE_WINDOW_SECS {
            e.fails = 0;
            e.first = now;
        }
        e.fails += 1;
        if e.fails >= MAX_FAILURES {
            e.locked_until = now + LOCKOUT_SECS;
            e.fails = 0;
            e.first = now;
            true
        } else {
            false
        }
    }

    /// A right token clears the address's record.
    pub fn success(&mut self, ip: Ipv4Addr) {
        self.map.remove(&ip);
    }

    /// Forget addresses whose window and lockout are over; if that is not enough, the oldest.
    fn prune(&mut self, now: f64) {
        self.map
            .retain(|_, e| e.locked_until > now || now - e.first <= FAILURE_WINDOW_SECS);
        while self.map.len() >= GUARD_CAP {
            let Some(oldest) = self
                .map
                .iter()
                .min_by(|a, b| a.1.first.total_cmp(&b.1.first))
                .map(|(k, _)| *k)
            else {
                break;
            };
            self.map.remove(&oldest);
        }
    }
}

// ----- who may talk to the PC ----------------------------------------------------------------

/// A private, link-local or loopback address (never a public one).
pub fn is_private_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback() || ip.is_link_local()
}

/// Do `a` and `b` share the first `prefix` bits?
pub fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let prefix = u32::from(prefix.min(32));
    if prefix == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - prefix);
    u32::from(a) & mask == u32::from(b) & mask
}

/// May a peer at `peer` talk to the PC? The PC itself always may (a tool on the same machine needs
/// the token too). Anything else must have a private address and, in `strict` mode, be on the same
/// network as one of the PC's own addresses (`local_nets`: address and prefix length), so the phone
/// on the same Wi-Fi is in and a private network reached through a router or VPN is out.
pub fn peer_allowed(peer: Ipv4Addr, local_nets: &[(Ipv4Addr, u8)], strict: bool) -> bool {
    if peer.is_loopback() {
        return true;
    }
    if !is_private_v4(peer) {
        return false;
    }
    !strict || local_nets.iter().any(|&(ip, p)| same_subnet(ip, peer, p))
}

// ----- what a request means ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// `GET /ping`: are you there, is the token right.
    Ping,
    /// `POST /clipboard`: text or a link for the clipboard history.
    Clipboard,
    /// `POST /notify`: a banner from the phone.
    Notify,
    /// `POST /battery`: the phone's battery.
    Battery,
    /// `POST /focus`: the phone's Focus.
    Focus,
    /// `POST /file`: a file for the shelf (the body is the file).
    File,
}

/// Which feature a route needs the PC to have switched on.
impl Route {
    pub fn needs(self) -> Option<&'static str> {
        match self {
            Route::Clipboard => Some("clipboard"),
            Route::File => Some("shelf"),
            Route::Notify => Some("notifications"),
            Route::Ping | Route::Battery | Route::Focus => None,
        }
    }
}

pub fn route(head: &Head) -> Result<Route, HttpError> {
    let r = match head.path.as_str() {
        "/ping" => Route::Ping,
        "/clipboard" => Route::Clipboard,
        "/notify" => Route::Notify,
        "/battery" => Route::Battery,
        "/focus" => Route::Focus,
        "/file" => Route::File,
        _ => return Err(HttpError::NotFound),
    };
    let want = if r == Route::Ping {
        Method::Get
    } else {
        Method::Post
    };
    if head.method == want {
        Ok(r)
    } else {
        Err(HttpError::MethodNotAllowed)
    }
}

/// How many body bytes to read for a route: the checked `Content-Length`. Refused before any body
/// byte is read: a missing length, or one over the limit.
pub fn body_length(route: Route, head: &Head, max_file_bytes: u64) -> Result<u64, HttpError> {
    if route == Route::Ping {
        return Ok(0);
    }
    let n = head.content_length.ok_or(HttpError::LengthRequired)?;
    let limit = if route == Route::File {
        max_file_bytes
    } else {
        MAX_JSON_BYTES as u64
    };
    if n > limit {
        return Err(HttpError::PayloadTooLarge);
    }
    Ok(n)
}

/// A request that is not a file, understood.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Clipboard(String),
    Notify {
        app: String,
        title: String,
        body: String,
    },
    Battery {
        percent: u8,
        charging: bool,
    },
    Focus {
        name: String,
        active: bool,
    },
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A percentage from a number, or from text like `82` and `82%` (Shortcuts often sends numbers as
/// text).
fn as_percent(v: &Value) -> Option<u8> {
    let n = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().trim_end_matches('%').trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (n.is_finite() && (0.0..=100.0).contains(&n)).then(|| n.round() as u8)
}

fn text_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match obj.get(*k) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    })
}

fn is_json(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|c| {
        let c = c.trim().to_ascii_lowercase();
        c.is_empty() || c.starts_with("application/json") || c.starts_with("text/json")
    })
}

/// What a JSON or text request asks for.
pub fn parse_body(
    route: Route,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<Action, HttpError> {
    if body.len() > MAX_JSON_BYTES {
        return Err(HttpError::PayloadTooLarge);
    }
    // Plain text is accepted for the clipboard: the body is the text.
    if route == Route::Clipboard
        && content_type.is_some_and(|c| c.trim().to_ascii_lowercase().starts_with("text/plain"))
    {
        let text = std::str::from_utf8(body)
            .map_err(|_| HttpError::BadRequest("the text is not valid UTF-8"))?;
        return clipboard_text(text.to_string());
    }
    if !is_json(content_type) {
        return Err(HttpError::UnsupportedMedia("send JSON (application/json)"));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| HttpError::BadRequest("the body is not valid JSON"))?;
    // A bare JSON string is the clipboard text.
    if route == Route::Clipboard
        && let Value::String(s) = &value
    {
        return clipboard_text(s.clone());
    }
    let Value::Object(obj) = &value else {
        return Err(HttpError::BadRequest("expected a JSON object"));
    };
    match route {
        Route::Clipboard => {
            let text = text_field(obj, &["text", "url", "link"])
                .ok_or(HttpError::BadRequest("expected {\"text\": \"...\"}"))?;
            clipboard_text(text)
        }
        Route::Battery => {
            let percent = obj
                .get("percent")
                .or_else(|| obj.get("level"))
                .and_then(as_percent)
                .ok_or(HttpError::BadRequest(
                    "expected {\"percent\": 0 to 100, \"charging\": true or false}",
                ))?;
            let charging = obj.get("charging").and_then(as_bool).unwrap_or(false);
            Ok(Action::Battery { percent, charging })
        }
        Route::Focus => {
            let active = obj
                .get("active")
                .and_then(as_bool)
                .ok_or(HttpError::BadRequest(
                    "expected {\"active\": true or false, \"name\": \"Work\"}",
                ))?;
            let name = tidy(&text_field(obj, &["name"]).unwrap_or_default(), 40);
            Ok(Action::Focus {
                name: if name.is_empty() {
                    "Focus".into()
                } else {
                    name
                },
                active,
            })
        }
        Route::Notify => {
            let title = tidy(&text_field(obj, &["title"]).unwrap_or_default(), 120);
            if title.is_empty() {
                return Err(HttpError::BadRequest(
                    "expected {\"title\": \"...\", \"body\": \"...\"}",
                ));
            }
            let body = tidy(&text_field(obj, &["body", "text"]).unwrap_or_default(), 300);
            let app = tidy(&text_field(obj, &["app"]).unwrap_or_default(), 60);
            Ok(Action::Notify {
                app: if app.is_empty() { "iPhone".into() } else { app },
                title,
                body,
            })
        }
        Route::Ping | Route::File => Err(HttpError::BadRequest("this address takes no JSON")),
    }
}

fn clipboard_text(text: String) -> Result<Action, HttpError> {
    let text: String = text.chars().filter(|&c| c != '\0').collect();
    if text.trim().is_empty() {
        return Err(HttpError::BadRequest("there is no text to add"));
    }
    Ok(Action::Clipboard(text))
}

// ----- files ---------------------------------------------------------------------------------

/// File types that are never accepted from the phone: programs and scripts. A phone sends photos,
/// documents and recordings; refusing these costs nothing and removes a way to hand someone a
/// program to double-click.
const BLOCKED_EXTENSIONS: &[&str] = &[
    "exe",
    "com",
    "scr",
    "bat",
    "cmd",
    "msi",
    "msp",
    "mst",
    "ps1",
    "psm1",
    "psd1",
    "vbs",
    "vbe",
    "js",
    "jse",
    "wsf",
    "wsh",
    "hta",
    "lnk",
    "url",
    "reg",
    "dll",
    "sys",
    "jar",
    "cpl",
    "inf",
    "scf",
    "appx",
    "msix",
    "appxbundle",
    "msixbundle",
    "gadget",
    "pif",
    "application",
    "chm",
    "iso",
    "img",
    "vhd",
    "vhdx",
    // Things Windows runs, loads or follows when they are opened: add-ins, snap-ins, scriptlets,
    // shell scraps and connectors (which can point at a network share), themes, remote-desktop
    // files, and scripts for interpreters people commonly have installed.
    "ocx",
    "drv",
    "msc",
    "sct",
    "ws",
    "wsc",
    "ps1xml",
    "psc1",
    "xll",
    "wll",
    "vsto",
    "diagcab",
    "settingcontent-ms",
    "library-ms",
    "searchconnector-ms",
    "theme",
    "themepack",
    "rdp",
    "jnlp",
    "shb",
    "shs",
    "vb",
    "vbscript",
    "py",
    "pyw",
    "pyz",
    "pl",
    "rb",
];

const RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A name a network peer sent, made safe to create as a file: no folders, no characters Windows
/// rejects, no device names, no leading or trailing dots and spaces, at most 100 characters (the
/// extension is kept). `None` if nothing usable is left.
pub fn sanitize_file_name(raw: &str) -> Option<String> {
    // Only the last component: the sender does not choose a folder.
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        // Characters that reorder text are how "invoice\u{202E}fdp.exe" is made.
        .filter(|c| !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'))
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']).to_string();
    if cleaned.is_empty() {
        return None;
    }
    let (stem, ext) = match cleaned.rfind('.') {
        Some(i) if i > 0 => (&cleaned[..i], &cleaned[i..]),
        _ => (cleaned.as_str(), ""),
    };
    let stem = stem.trim_end_matches([' ', '.']);
    if stem.is_empty() || RESERVED_NAMES.contains(&stem.to_ascii_lowercase().as_str()) {
        return None;
    }
    let ext: String = ext.chars().take(12).collect();
    let stem: String = stem
        .chars()
        .take(100usize.saturating_sub(ext.chars().count()).max(8))
        .collect();
    Some(format!("{}{}", stem.trim_end_matches([' ', '.']), ext))
}

fn extension_for(content_type: Option<&str>) -> &'static str {
    let ct = content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/heic" | "image/heif" => "heic",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/tiff" => "tiff",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        "video/quicktime" => "mov",
        "video/mp4" => "mp4",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" => "m4a",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/zip" => "zip",
        _ => "",
    }
}

/// The name a received file is saved under: the sender's (`?name=` or `X-File-Name`) cleaned up, or
/// `phone-<stamp>.<ext>` when it sent none. Programs and scripts are refused.
pub fn file_name_for(
    head: &Head,
    content_type: Option<&str>,
    stamp: &str,
) -> Result<String, HttpError> {
    let sent = head
        .query_param("name")
        .or_else(|| head.header("x-file-name"));
    let ext = extension_for(content_type);
    let mut name = match sent.and_then(sanitize_file_name) {
        Some(n) => n,
        None => {
            if sent.is_some_and(|s| !s.trim().is_empty()) {
                return Err(HttpError::BadRequest("that file name cannot be used"));
            }
            let safe_stamp: String = stamp
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(24)
                .collect();
            format!("phone-{safe_stamp}")
        }
    };
    // A name without an extension takes one from the content type, if there is one.
    if !name
        .rsplit('.')
        .next()
        .is_some_and(|e| name.contains('.') && !e.is_empty())
        && !ext.is_empty()
    {
        name = format!("{name}.{ext}");
    }
    if let Some(e) = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
        && BLOCKED_EXTENSIONS.contains(&e.as_str())
    {
        return Err(HttpError::UnsupportedMedia(
            "programs and scripts are not accepted from the phone",
        ));
    }
    Ok(name)
}

// ----- the pairing token ---------------------------------------------------------------------

/// Characters in a token.
pub const TOKEN_LEN: usize = 32;
/// 32 symbols without the ones that look alike (`i`, `l`, `o`, `u`), so a token can be read out or
/// retyped. Five bits each: 160 bits in all.
const TOKEN_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// A token from [`TOKEN_LEN`] random bytes. (256 is a multiple of 32, so taking five bits of each
/// byte is uniform: no symbol is likelier than another.)
pub fn token_from_bytes(bytes: &[u8; TOKEN_LEN]) -> String {
    bytes
        .iter()
        .map(|b| TOKEN_ALPHABET[usize::from(b & 31)] as char)
        .collect()
}

/// Is `s` a token this app could have made? (A token file that was edited or damaged is replaced.)
pub fn token_valid(s: &str) -> bool {
    s.len() == TOKEN_LEN && s.bytes().all(|b| TOKEN_ALPHABET.contains(&b))
}

// ----- where the PC can be reached -----------------------------------------------------------

/// One IPv4 address of this PC on a network that is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalNet {
    pub ip: Ipv4Addr,
    pub prefix: u8,
    /// The adapter has a default gateway: it reaches the router (the home network), unlike the
    /// virtual switches of WSL, Hyper-V or Docker.
    pub gateway: bool,
}

/// The `(address, prefix)` pairs [`peer_allowed`] wants.
pub fn net_pairs(nets: &[LocalNet]) -> Vec<(Ipv4Addr, u8)> {
    nets.iter().map(|n| (n.ip, n.prefix)).collect()
}

/// The addresses to show for a phone to use, as `ip:port`: private ones only (a phone cannot reach
/// anything else, and the listener would not accept it), the home network's first, at most four.
pub fn display_addrs(nets: &[LocalNet], port: u16) -> Vec<String> {
    let mut usable: Vec<&LocalNet> = nets
        .iter()
        .filter(|n| is_private_v4(n.ip) && !n.ip.is_loopback() && !n.ip.is_link_local())
        .collect();
    // Stable: addresses on adapters with a gateway move up, the rest keep the system's order.
    usable.sort_by_key(|n| !n.gateway);
    let mut out: Vec<String> = Vec::new();
    for n in usable {
        let a = format!("{}:{port}", n.ip);
        if !out.contains(&a) {
            out.push(a);
        }
        if out.len() == 4 {
            break;
        }
    }
    out
}

// ----- the inbox of received files -----------------------------------------------------------

/// What goes in a received file's `Zone.Identifier` stream: "came from the Internet zone". Windows
/// then shows SmartScreen and Office's Protected View for it, as for a browser download. (The PC
/// cannot know where the phone got the file, so the cautious zone is the honest one.)
pub const ZONE_IDENTIFIER: &str = "[ZoneTransfer]\r\nZoneId=3\r\n";

/// The folder one received file goes in: `<unix time>-<random>`. A folder per file keeps two files
/// of the same name apart without ever changing the name the sender gave.
pub fn inbox_dir_name(unix: i64, random: u32) -> String {
    format!("{:010}-{random:08x}", unix.max(0))
}

/// When an inbox folder was made, from its name. `None` for anything this app did not name (so
/// clearing out old files never touches a folder somebody else put there).
pub fn inbox_dir_time(name: &str) -> Option<i64> {
    let (time, random) = name.split_once('-')?;
    if time.len() != 10
        || random.len() != 8
        || !time.bytes().all(|b| b.is_ascii_digit())
        || !random.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    time.parse().ok()
}

/// Is this inbox folder older than `keep_days`?
pub fn inbox_expired(name: &str, now: i64, keep_days: u32) -> bool {
    inbox_dir_time(name).is_some_and(|t| now.saturating_sub(t) > i64::from(keep_days) * 86_400)
}

// ----- responses -----------------------------------------------------------------------------

/// The whole response for `status`, with a JSON `body`. Always `Connection: close`.
pub fn response(status: (u16, &str), body: &str, extra_headers: &[(&str, &str)]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        status.0,
        status.1,
        body.len()
    );
    for (n, v) in extra_headers {
        // Header values are ours, never the client's: a CR or LF would be a bug, so refuse it.
        if !n.contains(['\r', '\n', ':']) && !v.contains(['\r', '\n']) {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
    }
    out.push_str("\r\n");
    out.push_str(body);
    out.into_bytes()
}

/// The JSON body for success: `{"ok":true}` and any string fields (escaped properly).
pub fn ok_body(fields: &[(&str, &str)]) -> String {
    let mut m = serde_json::Map::new();
    m.insert("ok".into(), Value::Bool(true));
    for (k, v) in fields {
        m.insert((*k).into(), Value::String((*v).into()));
    }
    Value::Object(m).to_string()
}

/// The JSON body for an error: `{"ok":false,"error":"..."}`.
pub fn error_body(e: HttpError) -> String {
    serde_json::json!({ "ok": false, "error": e.message() }).to_string()
}

/// The full response for an error (a `401` also says how to authenticate, a `429` when to retry).
pub fn error_response(e: HttpError, retry_after_secs: Option<u64>) -> Vec<u8> {
    let (code, reason) = e.status();
    let retry = retry_after_secs.map(|s| s.to_string());
    let mut extra: Vec<(&str, &str)> = Vec::new();
    if e == HttpError::Unauthorized {
        extra.push(("WWW-Authenticate", "Bearer"));
    }
    if let Some(r) = &retry {
        extra.push(("Retry-After", r));
    }
    response((code, reason), &error_body(e), &extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(req: &str) -> Head {
        parse_head(req.as_bytes()).unwrap().unwrap()
    }

    fn post(path: &str, extra: &str) -> Head {
        head(&format!("POST {path} HTTP/1.1\r\nHost: pc\r\n{extra}\r\n"))
    }

    // ----- the head -----

    #[test]
    fn a_complete_request_head_is_read() {
        let h = head(
            "POST /clipboard?x=1&name=a%20b+c HTTP/1.1\r\nHost: pc:8765\r\nAuthorization: Bearer abc\r\nContent-Length: 12\r\nContent-Type: application/json\r\n\r\n{\"text\":\"x\"}",
        );
        assert_eq!(h.method, Method::Post);
        assert_eq!(h.path, "/clipboard");
        assert_eq!(h.query_param("x"), Some("1"));
        assert_eq!(h.query_param("name"), Some("a b c"));
        assert_eq!(h.content_length, Some(12));
        assert_eq!(h.header("CONTENT-TYPE"), Some("application/json"));
        assert_eq!(h.header("authorization"), Some("Bearer abc"));
        assert_eq!(h.header("nope"), None);
        // The body starts where the head ends.
        let req = "POST /x HTTP/1.1\r\nContent-Length: 1\r\n\r\nZ";
        assert_eq!(&req.as_bytes()[head(req).head_len..], b"Z");
    }

    #[test]
    fn an_incomplete_head_asks_for_more_and_a_runaway_one_is_refused() {
        assert_eq!(parse_head(b"GET /ping HTTP/1.1\r\nHo").unwrap(), None);
        assert_eq!(parse_head(b"").unwrap(), None);
        let junk = vec![b'a'; MAX_HEAD_BYTES];
        assert_eq!(parse_head(&junk), Err(HttpError::HeadTooLarge));
        let mut long = b"GET /ping HTTP/1.1\r\nX: ".to_vec();
        long.extend(vec![b'a'; MAX_HEAD_BYTES]);
        long.extend(b"\r\n\r\n");
        assert_eq!(parse_head(&long), Err(HttpError::HeadTooLarge));
    }

    #[test]
    fn malformed_requests_are_refused() {
        for bad in [
            "GET /ping\r\n\r\n",                               // no version
            "GET /ping HTTP/2\r\n\r\n",                        // another version
            "get /ping HTTP/1.1\r\n\r\n",                      // lower-case method
            "GET ping HTTP/1.1\r\n\r\n",                       // target without a slash
            "GET /pi ng HTTP/1.1\r\n\r\n",                     // a space in the target
            "GET /ping#frag HTTP/1.1\r\n\r\n",                 // a fragment
            "GET /ping HTTP/1.1 extra\r\n\r\n",                // a fourth part
            "GET /ping HTTP/1.1\r\nNoColon\r\n\r\n",           // a header without a colon
            "GET /ping HTTP/1.1\r\nBad Name: x\r\n\r\n",       // a space in the name
            "GET /ping HTTP/1.1\r\nName : x\r\n\r\n",          // a space before the colon
            "GET /ping HTTP/1.1\r\n folded: x\r\n\r\n",        // obsolete folding
            "GET /ping HTTP/1.1\r\nX: a\u{1}b\r\n\r\n",        // a control character
            "GET /ping?x=%zz HTTP/1.1\r\n\r\n",                // a broken escape
            "GET /ping?x=%ff HTTP/1.1\r\n\r\n",                // not UTF-8
            "POST /x HTTP/1.1\r\nContent-Length: -1\r\n\r\n",  // a negative length
            "POST /x HTTP/1.1\r\nContent-Length: 1e3\r\n\r\n", // not digits
            "POST /x HTTP/1.1\r\nContent-Length: 99999999999999999999\r\n\r\n",
            "POST /x HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n", // two lengths
        ] {
            assert_eq!(
                parse_head(bad.as_bytes()),
                Err(HttpError::Malformed),
                "{bad:?}"
            );
        }
        // The same length twice is harmless.
        assert!(
            parse_head(b"POST /x HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n")
                .is_ok()
        );
        // Chunked bodies are not supported, and say so.
        assert!(matches!(
            parse_head(b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Err(HttpError::BadRequest(_))
        ));
        // Not valid UTF-8 anywhere in the head.
        assert_eq!(
            parse_head(b"GET /\xff HTTP/1.1\r\n\r\n"),
            Err(HttpError::Malformed)
        );
    }

    #[test]
    fn too_many_headers_are_refused_and_other_methods_are_parsed_but_not_served() {
        let mut req = String::from("GET /ping HTTP/1.1\r\n");
        for i in 0..70 {
            req.push_str(&format!("X-{i}: v\r\n"));
        }
        req.push_str("\r\n");
        assert_eq!(parse_head(req.as_bytes()), Err(HttpError::HeadTooLarge));
        let h = head("DELETE /ping HTTP/1.1\r\n\r\n");
        assert_eq!(h.method, Method::Other);
        assert_eq!(route(&h), Err(HttpError::MethodNotAllowed));
    }

    #[test]
    fn expect_continue_and_percent_decoding() {
        assert!(post("/file", "Expect: 100-continue\r\n").expect_continue);
        assert!(!post("/file", "").expect_continue);
        assert_eq!(percent_decode("a%20b%2Fc", false), Some("a b/c".into()));
        assert_eq!(percent_decode("a+b", true), Some("a b".into()));
        assert_eq!(percent_decode("a+b", false), Some("a+b".into()));
        assert_eq!(percent_decode("%e2%9c%93", false), Some("✓".into()));
        assert_eq!(percent_decode("%", false), None);
        assert_eq!(percent_decode("%4", false), None);
    }

    #[test]
    fn nothing_the_network_sends_can_make_the_parser_panic() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let seeds: [&[u8]; 4] = [
            b"POST /file?name=a HTTP/1.1\r\nContent-Length: 3\r\nAuthorization: Bearer x\r\n\r\n",
            b"GET /ping HTTP/1.1\r\n\r\n",
            b"POST /clipboard HTTP/1.0\r\nExpect: 100-continue\r\n\r\n",
            b"",
        ];
        for round in 0..4000 {
            let mut buf = seeds[round % seeds.len()].to_vec();
            for _ in 0..(next() % 6) {
                let r = next();
                if buf.is_empty() || r % 3 == 0 {
                    buf.push((r >> 8) as u8);
                } else {
                    let i = (r >> 16) as usize % buf.len();
                    if r % 3 == 1 {
                        buf[i] = (r >> 24) as u8;
                    } else {
                        buf.truncate(i);
                    }
                }
            }
            let _ = parse_head(&buf);
        }
    }

    // ----- the token -----

    #[test]
    fn the_bearer_token_is_read_and_compared() {
        let h = head("GET /ping HTTP/1.1\r\nAuthorization: Bearer s3cret\r\n\r\n");
        assert_eq!(bearer_token(&h), Some("s3cret"));
        assert!(token_ok(&h, "s3cret"));
        assert!(!token_ok(&h, "s3cret2"));
        assert!(!token_ok(&h, ""), "an empty token never matches");
        let h = head("GET /ping HTTP/1.1\r\nauthorization: bearer   s3cret  \r\n\r\n");
        assert!(
            token_ok(&h, "s3cret"),
            "scheme case and spacing do not matter"
        );
        for none in [
            "GET /ping HTTP/1.1\r\n\r\n",
            "GET /ping HTTP/1.1\r\nAuthorization: Basic abc\r\n\r\n",
            "GET /ping HTTP/1.1\r\nAuthorization: Bearer\r\n\r\n",
            "GET /ping HTTP/1.1\r\nAuthorization: Bearer \r\n\r\n",
        ] {
            assert!(!token_ok(&head(none), "abc"), "{none:?}");
        }
        // The token in the address is not accepted: addresses end up in logs and histories.
        assert!(!token_ok(
            &head("GET /ping?token=s3cret HTTP/1.1\r\n\r\n"),
            "s3cret"
        ));
    }

    #[test]
    fn comparison_looks_at_everything() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"abcd", b"abc"));
        assert!(!ct_eq(b"", b"a"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn wrong_guesses_lock_an_address_out_for_a_while() {
        let mut g = LoginGuard::new();
        let a = Ipv4Addr::new(192, 168, 1, 50);
        let b = Ipv4Addr::new(192, 168, 1, 51);
        for i in 0..4 {
            assert!(!g.failure(a, f64::from(i)), "failure {i}");
        }
        assert_eq!(g.locked(a, 4.0), None);
        assert!(g.failure(a, 4.0), "the fifth locks it");
        let left = g.locked(a, 10.0).unwrap();
        assert!((left - (LOCKOUT_SECS - 6.0)).abs() < 1e-9);
        assert_eq!(g.locked(b, 10.0), None, "another address is not affected");
        assert_eq!(g.locked(a, 4.0 + LOCKOUT_SECS + 1.0), None, "and it ends");
        // Slow guessing never accumulates: failures outside the window start over.
        let mut g = LoginGuard::new();
        for i in 0..20 {
            assert!(!g.failure(a, f64::from(i) * (FAILURE_WINDOW_SECS + 1.0)));
        }
        // A right token forgives.
        let mut g = LoginGuard::new();
        for i in 0..4 {
            g.failure(a, f64::from(i));
        }
        g.success(a);
        for i in 0..4 {
            assert!(!g.failure(a, 10.0 + f64::from(i)));
        }
    }

    #[test]
    fn the_guard_stays_bounded_under_a_flood_of_addresses() {
        let mut g = LoginGuard::new();
        for i in 0..5000u32 {
            g.failure(Ipv4Addr::from(0x0a00_0000 + i), f64::from(i) * 0.01);
        }
        assert!(g.map.len() <= GUARD_CAP);
    }

    // ----- who may connect -----

    #[test]
    fn only_private_peers_on_the_same_network_may_talk() {
        let net = [(Ipv4Addr::new(192, 168, 1, 23), 24)];
        let ip = |a, b, c, d| Ipv4Addr::new(a, b, c, d);
        assert!(
            peer_allowed(ip(192, 168, 1, 77), &net, true),
            "the phone on the same Wi-Fi"
        );
        assert!(
            !peer_allowed(ip(192, 168, 2, 77), &net, true),
            "another private network"
        );
        assert!(
            peer_allowed(ip(192, 168, 2, 77), &net, false),
            "unless strictness is off"
        );
        assert!(
            !peer_allowed(ip(8, 8, 8, 8), &net, false),
            "never a public address"
        );
        assert!(
            !peer_allowed(ip(172, 32, 0, 1), &net, false),
            "172.32 is public"
        );
        assert!(peer_allowed(ip(172, 16, 5, 5), &net, false));
        assert!(peer_allowed(ip(10, 1, 2, 3), &net, false));
        assert!(peer_allowed(ip(169, 254, 3, 4), &net, false), "link-local");
        assert!(peer_allowed(ip(127, 0, 0, 1), &[], true), "this PC");
        assert!(
            !peer_allowed(ip(192, 168, 1, 77), &[], true),
            "no known network: strict says no"
        );
        assert!(same_subnet(ip(10, 0, 0, 1), ip(10, 255, 255, 255), 8));
        assert!(!same_subnet(ip(10, 0, 0, 1), ip(11, 0, 0, 1), 8));
        assert!(same_subnet(ip(1, 2, 3, 4), ip(200, 2, 3, 4), 0));
        assert!(same_subnet(ip(1, 2, 3, 4), ip(1, 2, 3, 4), 32));
        assert!(!same_subnet(ip(1, 2, 3, 4), ip(1, 2, 3, 5), 32));
    }

    // ----- routing -----

    #[test]
    fn fixed_addresses_and_methods_only() {
        let g = |p: &str| head(&format!("GET {p} HTTP/1.1\r\n\r\n"));
        assert_eq!(route(&g("/ping")), Ok(Route::Ping));
        assert_eq!(route(&g("/clipboard")), Err(HttpError::MethodNotAllowed));
        assert_eq!(route(&g("/nothing")), Err(HttpError::NotFound));
        assert_eq!(route(&g("/ping/")), Err(HttpError::NotFound));
        assert_eq!(route(&g("/../etc")), Err(HttpError::NotFound));
        for (p, r) in [
            ("/clipboard", Route::Clipboard),
            ("/notify", Route::Notify),
            ("/battery", Route::Battery),
            ("/focus", Route::Focus),
            ("/file", Route::File),
        ] {
            assert_eq!(route(&post(p, "")), Ok(r));
        }
        assert_eq!(route(&post("/ping", "")), Err(HttpError::MethodNotAllowed));
        assert_eq!(Route::Clipboard.needs(), Some("clipboard"));
        assert_eq!(Route::File.needs(), Some("shelf"));
        assert_eq!(Route::Battery.needs(), None);
    }

    #[test]
    fn body_lengths_are_checked_before_a_byte_is_read() {
        let cap = 10 * 1024 * 1024;
        assert_eq!(
            body_length(Route::Ping, &head("GET /ping HTTP/1.1\r\n\r\n"), cap),
            Ok(0)
        );
        assert_eq!(
            body_length(Route::Clipboard, &post("/clipboard", ""), cap),
            Err(HttpError::LengthRequired)
        );
        assert_eq!(
            body_length(
                Route::Clipboard,
                &post("/clipboard", "Content-Length: 100\r\n"),
                cap
            ),
            Ok(100)
        );
        assert_eq!(
            body_length(
                Route::Clipboard,
                &post("/clipboard", "Content-Length: 70000\r\n"),
                cap
            ),
            Err(HttpError::PayloadTooLarge)
        );
        assert_eq!(
            body_length(
                Route::File,
                &post("/file", "Content-Length: 10485760\r\n"),
                cap
            ),
            Ok(10 * 1024 * 1024)
        );
        assert_eq!(
            body_length(
                Route::File,
                &post("/file", "Content-Length: 10485761\r\n"),
                cap
            ),
            Err(HttpError::PayloadTooLarge)
        );
    }

    // ----- bodies -----

    #[test]
    fn clipboard_text_comes_as_json_a_bare_string_or_plain_text() {
        let p = |ct: Option<&str>, b: &str| parse_body(Route::Clipboard, ct, b.as_bytes());
        assert_eq!(
            p(Some("application/json"), r#"{"text":"hello"}"#),
            Ok(Action::Clipboard("hello".into()))
        );
        assert_eq!(
            p(None, r#"{"url":"https://a.b/c"}"#),
            Ok(Action::Clipboard("https://a.b/c".into()))
        );
        assert_eq!(
            p(Some("application/json; charset=utf-8"), r#""bare""#),
            Ok(Action::Clipboard("bare".into()))
        );
        assert_eq!(
            p(Some("text/plain; charset=utf-8"), "line 1\nline 2"),
            Ok(Action::Clipboard("line 1\nline 2".into()))
        );
        assert_eq!(
            p(Some("application/json"), r#"{"text":"a\u0000b"}"#),
            Ok(Action::Clipboard("ab".into())),
            "NULs are dropped"
        );
        for bad in [
            r#"{"text":""}"#,
            r#"{"text":"   "}"#,
            r#"{"text":5}"#,
            r#"{}"#,
            r#"[1]"#,
            "nope",
        ] {
            assert!(
                matches!(
                    p(Some("application/json"), bad),
                    Err(HttpError::BadRequest(_))
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            p(Some("text/plain"), ""),
            Err(HttpError::BadRequest(_))
        ));
        assert!(matches!(
            parse_body(Route::Clipboard, Some("text/plain"), &[0xff, 0xfe]),
            Err(HttpError::BadRequest(_))
        ));
        assert!(matches!(
            p(Some("image/png"), "x"),
            Err(HttpError::UnsupportedMedia(_))
        ));
        assert_eq!(
            parse_body(Route::Clipboard, None, &vec![b'a'; MAX_JSON_BYTES + 1]),
            Err(HttpError::PayloadTooLarge)
        );
    }

    #[test]
    fn battery_accepts_numbers_and_the_text_shortcuts_sends() {
        let p = |b: &str| parse_body(Route::Battery, Some("application/json"), b.as_bytes());
        assert_eq!(
            p(r#"{"percent":82,"charging":true}"#),
            Ok(Action::Battery {
                percent: 82,
                charging: true
            })
        );
        assert_eq!(
            p(r#"{"percent":"82%","charging":"yes"}"#),
            Ok(Action::Battery {
                percent: 82,
                charging: true
            })
        );
        assert_eq!(
            p(r#"{"level":" 7 "}"#),
            Ok(Action::Battery {
                percent: 7,
                charging: false
            })
        );
        assert_eq!(
            p(r#"{"percent":81.6}"#),
            Ok(Action::Battery {
                percent: 82,
                charging: false
            })
        );
        assert_eq!(
            p(r#"{"percent":0,"charging":0}"#),
            Ok(Action::Battery {
                percent: 0,
                charging: false
            })
        );
        for bad in [
            r#"{"percent":101}"#,
            r#"{"percent":-1}"#,
            r#"{"percent":"lots"}"#,
            r#"{"percent":null}"#,
            r#"{}"#,
            r#"{"percent":1e999}"#,
        ] {
            assert!(matches!(p(bad), Err(HttpError::BadRequest(_))), "{bad}");
        }
    }

    #[test]
    fn focus_and_notify_are_cleaned_and_bounded() {
        let f = |b: &str| parse_body(Route::Focus, Some("application/json"), b.as_bytes());
        assert_eq!(
            f(r#"{"active":true,"name":"Work"}"#),
            Ok(Action::Focus {
                name: "Work".into(),
                active: true
            })
        );
        assert_eq!(
            f(r#"{"active":"false"}"#),
            Ok(Action::Focus {
                name: "Focus".into(),
                active: false
            })
        );
        assert!(matches!(
            f(r#"{"name":"Work"}"#),
            Err(HttpError::BadRequest(_))
        ));
        let Ok(Action::Focus { name, .. }) = f(&format!(
            r#"{{"active":true,"name":"{}"}}"#,
            "x".repeat(300)
        )) else {
            panic!("long names are cut, not refused");
        };
        assert!(name.chars().count() <= 41);

        let n = |b: &str| parse_body(Route::Notify, Some("application/json"), b.as_bytes());
        assert_eq!(
            n(r#"{"app":"Messages","title":"Mum","body":"Dinner\n on   Sunday?"}"#),
            Ok(Action::Notify {
                app: "Messages".into(),
                title: "Mum".into(),
                body: "Dinner on Sunday?".into()
            })
        );
        assert_eq!(
            n(r#"{"title":"Ping"}"#),
            Ok(Action::Notify {
                app: "iPhone".into(),
                title: "Ping".into(),
                body: String::new()
            })
        );
        assert!(matches!(
            n(r#"{"body":"no title"}"#),
            Err(HttpError::BadRequest(_))
        ));
        assert!(matches!(
            n(r#"{"title":"  \n "}"#),
            Err(HttpError::BadRequest(_))
        ));
        assert!(matches!(
            parse_body(Route::File, Some("application/json"), b"{}"),
            Err(HttpError::BadRequest(_))
        ));
    }

    // ----- file names -----

    #[test]
    fn file_names_from_the_network_become_safe_names() {
        let ok = |raw: &str| sanitize_file_name(raw);
        assert_eq!(ok("photo.jpg"), Some("photo.jpg".into()));
        assert_eq!(ok("IMG 0042.HEIC"), Some("IMG 0042.HEIC".into()));
        assert_eq!(
            ok("../../Windows/System32/evil.txt"),
            Some("evil.txt".into())
        );
        assert_eq!(ok(r"C:\Users\x\notes.txt"), Some("notes.txt".into()));
        assert_eq!(ok("a<b>c:d\"e|f?g*h.txt"), Some("abcdefgh.txt".into()));
        assert_eq!(ok("  .hidden. "), Some("hidden".into()));
        assert_eq!(ok("invoice\u{202E}fdp.exe"), Some("invoicefdp.exe".into()));
        assert_eq!(ok("a\u{7}b\nc.txt"), Some("abc.txt".into()));
        for bad in [
            "", "   ", "...", "CON", "con.txt", "NUL.png", "lpt1", "COM9.tar", "/", "a/", "..\\..",
        ] {
            assert_eq!(ok(bad), None, "{bad:?}");
        }
        let long = format!("{}.jpeg", "x".repeat(300));
        let cut = ok(&long).unwrap();
        assert!(
            cut.chars().count() <= 106 && cut.ends_with(".jpeg"),
            "{cut}"
        );
        assert_eq!(ok("trailing. "), Some("trailing".into()));
    }

    #[test]
    fn a_received_file_gets_the_senders_name_or_one_of_ours() {
        let name = |q: &str, ct: Option<&str>| {
            file_name_for(&post(&format!("/file{q}"), ""), ct, "20261006-153012")
        };
        assert_eq!(
            name("?name=holiday.jpg", Some("image/jpeg")),
            Ok("holiday.jpg".into())
        );
        assert_eq!(
            name("?name=holiday", Some("image/jpeg")),
            Ok("holiday.jpg".into()),
            "extension from the type"
        );
        assert_eq!(
            name("", Some("application/pdf")),
            Ok("phone-20261006-153012.pdf".into())
        );
        assert_eq!(name("", None), Ok("phone-20261006-153012".into()));
        assert_eq!(name("?name=a%2Fb%2Fc.txt", None), Ok("c.txt".into()));
        let h = head("POST /file HTTP/1.1\r\nX-File-Name: from header.png\r\n\r\n");
        assert_eq!(
            file_name_for(&h, Some("image/png"), "s"),
            Ok("from header.png".into())
        );
        // A name that is nothing usable is refused rather than silently replaced.
        assert!(matches!(
            name("?name=CON", None),
            Err(HttpError::BadRequest(_))
        ));
        // The stamp cannot smuggle path characters.
        assert_eq!(
            file_name_for(&post("/file", ""), None, "..\\evil"),
            Ok("phone-evil".into())
        );
    }

    #[test]
    fn programs_and_scripts_are_refused_whatever_they_are_called() {
        for n in [
            "setup.exe",
            "run.BAT",
            "x.ps1",
            "a.lnk",
            "readme.txt.exe",
            "d.dll",
            "m.msi",
            "s.js",
            "i.iso",
            "p.hta",
            "addin.XLL",
            "calc.msc",
            "tool.py",
            "photos.library-ms",
            "look.theme",
            "remote.rdp",
            "disk.img",
            "invoice.pdf.exe.",
            "invoice.pdf.exe%20",
        ] {
            let h = post(&format!("/file?name={n}"), "");
            assert!(
                matches!(
                    file_name_for(&h, None, "s"),
                    Err(HttpError::UnsupportedMedia(_))
                ),
                "{n}"
            );
        }
        for n in [
            "a.jpg", "b.pdf", "c.docx", "d.txt", "e.zip", "f.heic", "g.mov",
        ] {
            let h = post(&format!("/file?name={n}"), "");
            assert!(file_name_for(&h, None, "s").is_ok(), "{n}");
        }
    }

    // ----- responses -----

    #[test]
    fn responses_are_well_formed_and_say_nothing_about_the_token() {
        let r = String::from_utf8(error_response(HttpError::Unauthorized, None)).unwrap();
        assert!(r.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(r.contains("WWW-Authenticate: Bearer\r\n") && r.contains("Connection: close\r\n"));
        assert!(r.ends_with(r#"{"error":"the token was not accepted","ok":false}"#));
        let len: usize = r
            .split("Content-Length: ")
            .nth(1)
            .unwrap()
            .split("\r\n")
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(len, r.split("\r\n\r\n").nth(1).unwrap().len());
        let r = String::from_utf8(error_response(HttpError::Locked, Some(95))).unwrap();
        assert!(r.starts_with("HTTP/1.1 429 ") && r.contains("Retry-After: 95\r\n"));
        // A CR or LF in a header value is refused rather than sent.
        let r = String::from_utf8(response(
            (200, "OK"),
            "{}",
            &[("X-A", "b\r\nSet-Cookie: x")],
        ))
        .unwrap();
        assert!(!r.contains("Set-Cookie"));
        for e in [
            HttpError::Malformed,
            HttpError::HeadTooLarge,
            HttpError::NotFound,
            HttpError::MethodNotAllowed,
            HttpError::LengthRequired,
            HttpError::PayloadTooLarge,
            HttpError::Disabled("off"),
            HttpError::UnsupportedMedia("no"),
        ] {
            let (code, _) = e.status();
            assert!((400..500).contains(&code));
            assert!(!e.message().is_empty());
        }
        // A full disk is the PC's problem, not the phone's: a server error, with its own words.
        let r = String::from_utf8(error_response(
            HttpError::Storage("the inbox is full"),
            None,
        ))
        .unwrap();
        assert!(r.starts_with("HTTP/1.1 507 Insufficient Storage\r\n"));
        assert!(r.ends_with(r#"{"error":"the inbox is full","ok":false}"#));
    }

    // ----- the pairing token -----

    #[test]
    fn tokens_use_every_bit_and_only_readable_characters() {
        let t = token_from_bytes(&[0; TOKEN_LEN]);
        assert_eq!(t, "0".repeat(TOKEN_LEN));
        let t = token_from_bytes(&[0xFF; TOKEN_LEN]);
        assert_eq!(t, "z".repeat(TOKEN_LEN));
        // Every byte value maps to a symbol of the alphabet, and every symbol is reachable.
        let mut seen = std::collections::HashSet::new();
        for b in 0..=255u8 {
            let t = token_from_bytes(&[b; TOKEN_LEN]);
            assert!(token_valid(&t));
            seen.insert(t.chars().next().unwrap());
        }
        assert_eq!(seen.len(), 32);
        assert!(!seen.iter().any(|c| "ilou".contains(*c)), "look-alikes");
    }

    #[test]
    fn a_damaged_token_file_is_not_a_token() {
        let good = token_from_bytes(&[12; TOKEN_LEN]); // "cccc..."
        assert!(token_valid(&good));
        assert!(!token_valid(""));
        assert!(!token_valid(&good[..TOKEN_LEN - 1]));
        assert!(!token_valid(&format!("{good}0")));
        assert!(!token_valid(&good.to_uppercase()));
        assert!(!token_valid(&format!("{}!", &good[..TOKEN_LEN - 1])));
        assert!(!token_valid(&good.replace('c', "i")), "look-alike symbol");
        assert!(!token_valid(&format!(" {}", &good[1..])));
    }

    // ----- addresses -----

    fn net(a: [u8; 4], prefix: u8, gateway: bool) -> LocalNet {
        LocalNet {
            ip: Ipv4Addr::from(a),
            prefix,
            gateway,
        }
    }

    #[test]
    fn the_home_network_is_listed_first_and_only_usable_addresses_are() {
        let nets = [
            net([172, 20, 80, 1], 20, false), // WSL's virtual switch
            net([169, 254, 3, 4], 16, true),  // no DHCP answer: useless to a phone
            net([192, 168, 1, 20], 24, true), // the Wi-Fi
            net([127, 0, 0, 1], 8, false),
            net([203, 0, 113, 9], 24, true), // a public address: never offered
            net([192, 168, 1, 20], 24, true), // listed twice by the system
            net([10, 0, 0, 5], 8, false),
        ];
        assert_eq!(
            display_addrs(&nets, 8765),
            vec!["192.168.1.20:8765", "172.20.80.1:8765", "10.0.0.5:8765"]
        );
        assert!(display_addrs(&[], 1).is_empty());
        let many: Vec<LocalNet> = (1..=9).map(|i| net([10, 0, i, 1], 24, false)).collect();
        assert_eq!(display_addrs(&many, 80).len(), 4, "at most four");
    }

    #[test]
    fn the_pairs_for_the_peer_check_keep_every_network() {
        let nets = [
            net([192, 168, 1, 20], 24, true),
            net([10, 1, 2, 3], 16, false),
        ];
        let pairs = net_pairs(&nets);
        assert_eq!(pairs.len(), 2);
        assert!(peer_allowed(Ipv4Addr::new(192, 168, 1, 77), &pairs, true));
        assert!(peer_allowed(Ipv4Addr::new(10, 1, 200, 9), &pairs, true));
        assert!(!peer_allowed(Ipv4Addr::new(192, 168, 2, 77), &pairs, true));
    }

    // ----- the inbox -----

    #[test]
    fn inbox_folders_are_named_by_time_and_only_our_names_expire() {
        let n = inbox_dir_name(1_700_000_000, 0xAB12);
        assert_eq!(n, "1700000000-0000ab12");
        assert_eq!(inbox_dir_time(&n), Some(1_700_000_000));
        let day = 86_400;
        assert!(
            !inbox_expired(&n, 1_700_000_000 + 14 * day, 14),
            "exactly 14 days is kept"
        );
        assert!(inbox_expired(&n, 1_700_000_000 + 14 * day + 1, 14));
        assert!(
            !inbox_expired(&n, 1_600_000_000, 14),
            "a clock that went back keeps it"
        );
        // Anything else in the folder is somebody else's: never expired, never touched.
        for other in [
            "",
            "notes",
            "1700000000",
            "1700000000-",
            "1700000000-zzzzzzzz",
            "170000000-0000ab12",
            "17000000000-0000ab12",
            "1700000000-0000ab12-x",
            "-700000000-0000ab12",
            "1700000000-0000ab1",
            "..",
        ] {
            assert_eq!(inbox_dir_time(other), None, "{other:?}");
            assert!(!inbox_expired(other, i64::MAX, 1), "{other:?}");
        }
        // A negative clock does not produce a name that parses as something else.
        assert_eq!(inbox_dir_name(-5, 1), "0000000000-00000001");
    }

    #[test]
    fn success_bodies_escape_what_they_carry() {
        assert_eq!(ok_body(&[]), r#"{"ok":true}"#);
        assert_eq!(
            ok_body(&[("saved", "a \"b\".txt"), ("name", "Shark")]),
            r#"{"name":"Shark","ok":true,"saved":"a \"b\".txt"}"#
        );
    }

    #[test]
    fn received_files_are_marked_as_from_the_internet() {
        assert!(ZONE_IDENTIFIER.starts_with("[ZoneTransfer]\r\n"));
        assert!(ZONE_IDENTIFIER.contains("ZoneId=3\r\n"));
    }
}
