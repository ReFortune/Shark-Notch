//! The self-test's "phone": a client that talks to the real listener over loopback the way
//! Shortcuts would (plain HTTP, `Authorization: Bearer`), and reports what came back.
//!
//! It runs on its own thread, so the UI thread never waits on a socket while the script goes on.
//! Each check is a `(passed, what was seen)` line for the report.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One check: did it pass, and what was seen.
pub type Line = (bool, String);

struct Reply {
    status: u16,
    head: String,
    body: String,
}

struct Request<'a> {
    method: &'a str,
    path: &'a str,
    token: Option<&'a str>,
    content_type: Option<&'a str>,
    /// Extra header lines, each ending in `\r\n`.
    headers: String,
    body: &'a [u8],
    /// The `Content-Length` to promise, when it is not the body's length (a request that promises
    /// more than it sends).
    claim: Option<usize>,
}

fn get<'a>(path: &'a str, token: Option<&'a str>) -> Request<'a> {
    Request {
        method: "GET",
        path,
        token,
        content_type: None,
        headers: String::new(),
        body: &[],
        claim: None,
    }
}

fn post<'a>(path: &'a str, token: &'a str, content_type: &'a str, body: &'a [u8]) -> Request<'a> {
    Request {
        method: "POST",
        path,
        token: Some(token),
        content_type: Some(content_type),
        headers: String::new(),
        body,
        claim: None,
    }
}

fn json<'a>(path: &'a str, token: &'a str, body: &'a str) -> Request<'a> {
    post(path, token, "application/json", body.as_bytes())
}

/// Send one request to the listener and read the whole answer (the server closes the connection).
fn send(port: u16, r: &Request) -> Result<Reply, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| format!("cannot connect: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(8)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(8)));
    let mut head = format!("{} {} HTTP/1.1\r\nHost: 127.0.0.1\r\n", r.method, r.path);
    if let Some(t) = r.token {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(c) = r.content_type {
        head.push_str(&format!("Content-Type: {c}\r\n"));
    }
    head.push_str(&r.headers);
    if r.method == "POST" {
        head.push_str(&format!(
            "Content-Length: {}\r\n",
            r.claim.unwrap_or(r.body.len())
        ));
    }
    head.push_str("Connection: close\r\n\r\n");
    s.write_all(head.as_bytes())
        .map_err(|e| format!("cannot send: {e}"))?;
    // The server may answer (and stop listening) before the body is out: that is not an error.
    let _ = s.write_all(r.body);
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => {
                if out.is_empty() {
                    return Err(format!("cannot read the answer: {e}"));
                }
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no HTTP status in {head:?}"))?;
    Ok(Reply {
        status,
        head: head.to_string(),
        body: body.to_string(),
    })
}

struct Probe {
    port: u16,
    lines: Vec<Line>,
}

impl Probe {
    /// Send a request and expect `want` as the status.
    fn expect(&mut self, label: &str, req: &Request, want: u16) -> Option<Reply> {
        match send(self.port, req) {
            Ok(r) => {
                self.lines.push((
                    r.status == want,
                    format!("{label}: {} (expected {want})", r.status),
                ));
                Some(r)
            }
            Err(e) => {
                self.lines.push((false, format!("{label}: {e}")));
                None
            }
        }
    }

    fn check(&mut self, ok: bool, what: String) {
        self.lines.push((ok, what));
    }
}

/// Where a received file named `name` was saved: `<inbox>/<folder>/<name>`.
fn find_saved(inbox: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(inbox)
        .ok()?
        .flatten()
        .map(|d| d.path().join(name))
        .find(|p| p.is_file())
}

/// Everything a phone can do, and everything it must not be able to do, in one run. The lockout
/// comes last: it shuts this address out for a while.
pub fn exchange(port: u16, token: &str, inbox: &Path) -> Vec<Line> {
    let mut p = Probe {
        port,
        lines: Vec::new(),
    };

    // Nothing is learned without the token.
    p.expect("GET /ping without a token", &get("/ping", None), 401);
    p.expect(
        "GET /nothing without a token (which paths exist is not revealed)",
        &get("/nothing", None),
        401,
    );
    p.expect(
        "GET /ping with a wrong token",
        &get("/ping", Some("not-the-token")),
        401,
    );
    if let Some(r) = p.expect("GET /ping with the token", &get("/ping", Some(token)), 200) {
        let ok = r.body.contains("Shark Notch");
        p.check(ok, format!("the connection test names the app: {ok}"));
    }

    // What the phone sends.
    p.expect(
        "POST /clipboard (JSON text)",
        &json("/clipboard", token, r#"{"text":"from the phone"}"#),
        200,
    );
    p.expect(
        "POST /battery",
        &json("/battery", token, r#"{"percent":73,"charging":true}"#),
        200,
    );
    p.expect(
        "POST /focus",
        &json("/focus", token, r#"{"name":"Work","active":true}"#),
        200,
    );
    p.expect(
        "POST /notify",
        &json(
            "/notify",
            token,
            r#"{"app":"Messages","title":"Mum","body":"Call me"}"#,
        ),
        200,
    );

    // A file: saved under the inbox, marked as downloaded from the internet, in a folder of its own.
    let hello = b"hello from the phone";
    p.expect(
        "POST /file?name=hello.txt",
        &post("/file?name=hello.txt", token, "text/plain", hello),
        200,
    );
    match find_saved(inbox, "hello.txt") {
        Some(path) => {
            let same = std::fs::read(&path).is_ok_and(|b| b == hello);
            p.check(
                same,
                format!("the saved file has the bytes that were sent: {same}"),
            );
            let folder = path
                .parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let named = notch_core::phone::inbox_dir_time(folder).is_some();
            p.check(
                named,
                format!("it is in a folder named by time and chance ({folder}): {named}"),
            );
            let zone = std::fs::read_to_string(format!("{}:Zone.Identifier", path.display()))
                .unwrap_or_default();
            let marked = zone.contains("ZoneId=3");
            p.check(
                marked,
                format!("it carries the Zone.Identifier stream (ZoneId=3): {marked} ({zone:?})"),
            );
        }
        None => p.check(false, "the file was not saved in the inbox".into()),
    }

    // A name that tries to leave the inbox is cut down to its last part.
    let mut sneaky = post("/file", token, "text/plain", b"x");
    sneaky.headers = "X-File-Name: ../../evil.txt\r\n".into();
    if let Some(r) = p.expect("POST /file named ../../evil.txt", &sneaky, 200) {
        let plain = r.body.contains(r#""saved":"evil.txt""#);
        let outside = inbox.join("../evil.txt").exists()
            || inbox.parent().is_some_and(|d| d.join("evil.txt").exists());
        p.check(
            plain && !outside && find_saved(inbox, "evil.txt").is_some(),
            format!(
                "the name was cut to evil.txt ({plain}), nothing was written outside the inbox ({})",
                !outside
            ),
        );
    }

    // What must be refused.
    p.expect(
        "POST /file?name=tool.exe (programs are not accepted)",
        &post(
            "/file?name=tool.exe",
            token,
            "application/octet-stream",
            b"MZ",
        ),
        415,
    );
    let mut huge = post("/file?name=big.bin", token, "application/octet-stream", &[]);
    huge.claim = Some(5 * 1024 * 1024);
    p.expect(
        "POST /file promising 5 MiB (the limit is 1 MiB; refused before the body)",
        &huge,
        413,
    );
    p.expect(
        "POST /clipboard as XML",
        &post("/clipboard", token, "application/xml", b"<a/>"),
        415,
    );
    p.expect(
        "POST /clipboard with broken JSON",
        &json("/clipboard", token, "{not json"),
        400,
    );
    p.expect(
        "POST /battery without a percentage",
        &json("/battery", token, r#"{"charging":true}"#),
        400,
    );
    p.expect(
        "GET /nothing with the token",
        &get("/nothing", Some(token)),
        404,
    );
    p.expect(
        "GET /clipboard (it takes POST)",
        &get("/clipboard", Some(token)),
        405,
    );
    let mut long_head = get("/ping", Some(token));
    long_head.headers = format!("X-Padding: {}\r\n", "a".repeat(9000));
    p.expect("a request head of 9 KB", &long_head, 431);

    // Wrong guesses lock the address out, and the right token does not get it back in.
    for n in 1..=5 {
        p.expect(
            &format!("wrong token, guess {n}"),
            &get("/ping", Some("wrong-guess")),
            401,
        );
    }
    if let Some(r) = p.expect(
        "the right token while locked out",
        &get("/ping", Some(token)),
        429,
    ) {
        let retry = r.head.to_ascii_lowercase().contains("retry-after: ");
        p.check(
            retry,
            format!("the lockout says when to try again: {retry}"),
        );
    }
    p.lines
}

/// After "New token": the old one is dead, the new one works at once (which also shows that the
/// lockout of the first run was cleared).
pub fn after_new_token(port: u16, old: &str, new: &str) -> Vec<Line> {
    let mut p = Probe {
        port,
        lines: Vec::new(),
    };
    p.expect(
        "the new token, right after the change",
        &get("/ping", Some(new)),
        200,
    );
    p.expect(
        "the old token after the change",
        &get("/ping", Some(old)),
        401,
    );
    p.lines
}

/// Is the port closed? (After the link is switched off nothing may answer on it.)
pub fn port_closed(port: u16) -> Line {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
        Ok(_) => (
            false,
            format!("port {port} still accepts connections after the link was switched off"),
        ),
        Err(e) => (
            true,
            format!("port {port} refuses connections once the link is switched off ({e})"),
        ),
    }
}
