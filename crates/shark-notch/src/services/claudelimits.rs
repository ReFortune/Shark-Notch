//! Claude's plan limits (the 5-hour and weekly bars), opt-in: `[stats] ai_limits`.
//!
//! One worker thread that sleeps until the stats page asks. It reads the login Claude Code keeps in
//! `~/.claude/.credentials.json` and asks `api.anthropic.com/api/oauth/usage` (an endpoint
//! Anthropic has not documented for third parties) for the two windows, then sends one `AiLimits`
//! event. At most one request a minute (five after a failure); in between it repeats the last
//! answer. The token is read for each request, sent only in that request's `Authorization` header
//! and never logged, stored or put on the bus. If the token has run out the answer is `Expired`
//! (Claude Code renews it the next time it is used); this service never refreshes or writes it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notch_core::bus::BusSender;
use notch_core::claudelimits::{Credentials, LimitsState, parse_usage};
use notch_core::events::{EventKind, Source};

use crate::win::http;

const HOST: &str = "api.anthropic.com";
const PATH: &str = "/api/oauth/usage";
const MAX_BYTES: usize = 64 * 1024;
const EVERY: Duration = Duration::from_secs(60);
const EVERY_AFTER_FAILURE: Duration = Duration::from_secs(300);

pub struct ClaudeLimitsService {
    tx: Sender<()>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
}

fn credentials_path() -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".claude")))?;
    Some(base.join(".credentials.json"))
}

impl ClaudeLimitsService {
    pub fn start(bus: BusSender) -> Option<ClaudeLimitsService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let (d2, q2) = (done.clone(), quit.clone());
        let thread = std::thread::Builder::new()
            .name("claude-limits".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(&rx, &bus, &q2);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the Claude limits reader: {e}"))
            .ok()?;
        Some(ClaudeLimitsService {
            tx,
            thread: Some(thread),
            done,
            quit,
        })
    }

    pub fn refresh(&self) {
        let _ = self.tx.send(());
    }

    pub fn stop(mut self) {
        self.quit.store(true, Ordering::Release);
        let _ = self.tx.send(());
        if let Some(h) = self.thread.take() {
            // A request in flight can take its full timeout; do not wait for it.
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

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn run(rx: &Receiver<()>, bus: &BusSender, quit: &AtomicBool) {
    let mut last: Option<(Instant, LimitsState, Duration)> = None;
    while rx.recv().is_ok() {
        while rx.try_recv().is_ok() {}
        if quit.load(Ordering::Acquire) {
            return;
        }
        let state = match &last {
            Some((at, state, wait)) if at.elapsed() < *wait => state.clone(),
            _ => {
                let state = fetch();
                let wait = if state == LimitsState::Failed {
                    EVERY_AFTER_FAILURE
                } else {
                    EVERY
                };
                last = Some((Instant::now(), state.clone(), wait));
                state
            }
        };
        bus.send(Source::Local, EventKind::AiLimits(Arc::new(state)));
    }
}

fn fetch() -> LimitsState {
    let Some(creds) = credentials_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| Credentials::parse(&t))
    else {
        return LimitsState::SignedOut;
    };
    if creds.expired(unix_now()) {
        return LimitsState::Expired;
    }
    let bearer = creds.bearer();
    let result = http::get_https_with(
        HOST,
        443,
        PATH,
        MAX_BYTES,
        "application/json",
        &[
            ("Authorization", &bearer),
            ("anthropic-beta", "oauth-2025-04-20"),
        ],
    );
    match result {
        Ok(body) => parse_usage(&body).map_or_else(
            || {
                crate::debug!("claude limits: the answer was not understood");
                LimitsState::Failed
            },
            LimitsState::Known,
        ),
        Err(e) if e == "HTTP 401" || e == "HTTP 403" => LimitsState::Expired,
        Err(e) => {
            crate::debug!("claude limits: the request failed ({e})");
            LimitsState::Failed
        }
    }
}
