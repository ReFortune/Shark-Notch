//! Lyrics lookup on LRCLIB (opt-in: `[media] lyrics`).
//!
//! One worker thread that sleeps until a `LyricsCmd::Fetch` arrives, asks `lrclib.net` for the
//! track (the exact lookup first, the looser search if that finds nothing: at most two requests,
//! one after the other, never in parallel), and sends one `Lyrics` event with the answer. Requests
//! that pile up while one is running collapse into the newest. Nothing is written to disk; the
//! words live in the media module for the current track only.
//!
//! What goes over the network: the artist, title, album and length of the track, in the URL, to
//! `lrclib.net`, over HTTPS (WinHTTP, the system's TLS), with a `SharkNotch/<version>` user agent.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notch_core::bus::BusSender;
use notch_core::events::{EventKind, Source};
use notch_core::lyrics::{self, Lyrics, LyricsReply};
use notch_core::module::LyricsCmd;

use crate::win::http;

const HOST: &str = "lrclib.net";
/// A lyrics answer larger than this is not one (bounds memory against a misbehaving server).
const MAX_BYTES: usize = 512 * 1024;
const ACCEPT: &str = "application/json";

enum Req {
    Fetch(LyricsCmd),
    Quit,
}

pub struct LyricsService {
    tx: Sender<Req>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

impl LyricsService {
    pub fn start(bus: BusSender) -> Option<LyricsService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let thread = std::thread::Builder::new()
            .name("lyrics".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(&rx, &bus);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the lyrics lookup: {e}"))
            .ok()?;
        Some(LyricsService {
            tx,
            thread: Some(thread),
            done,
        })
    }

    pub fn command(&self, cmd: &LyricsCmd) {
        let _ = self.tx.send(Req::Fetch(cmd.clone()));
    }

    pub fn stop(mut self) {
        let _ = self.tx.send(Req::Quit);
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

fn run(rx: &Receiver<Req>, bus: &BusSender) {
    while let Ok(first) = rx.recv() {
        let mut latest = first;
        // Only the newest request matters; the player has moved on from the others.
        while let Ok(more) = rx.try_recv() {
            latest = more;
        }
        let Req::Fetch(LyricsCmd::Fetch {
            track,
            artist,
            title,
            album,
            duration_s,
        }) = latest
        else {
            return;
        };
        let lyrics = lookup(&artist, &title, &album, duration_s);
        bus.send(
            Source::Local,
            EventKind::Lyrics(Arc::new(LyricsReply { track, lyrics })),
        );
    }
}

/// The exact lookup, then the search when LRCLIB has no record under those exact words.
fn lookup(artist: &str, title: &str, album: &str, duration_s: u32) -> Lyrics {
    match http::get_https_accepting(
        HOST,
        443,
        &lyrics::get_path(artist, title, album, duration_s),
        MAX_BYTES,
        ACCEPT,
    ) {
        Ok(body) => lyrics::parse_get(&body),
        Err(e) if e == "HTTP 404" => match http::get_https_accepting(
            HOST,
            443,
            &lyrics::search_path(artist, title),
            MAX_BYTES,
            ACCEPT,
        ) {
            Ok(body) => lyrics::parse_search(&body),
            Err(e) => failed(&e),
        },
        Err(e) => failed(&e),
    }
}

fn failed(why: &str) -> Lyrics {
    crate::debug!("lyrics: the lookup failed ({why})");
    Lyrics::Failed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to the real lrclib.net (one lookup). Run with `cargo test -- --ignored lrclib`.
    #[test]
    #[ignore = "needs the network"]
    fn lrclib_answers_a_well_known_track() {
        let l = lookup("Queen", "Bohemian Rhapsody", "A Night at the Opera", 354);
        eprintln!(
            "lrclib: {}",
            match &l {
                Lyrics::Synced(lines) =>
                    format!("synced, {} lines, first {:?}", lines.len(), lines.first()),
                other => format!("{other:?}"),
            }
        );
        assert!(matches!(l, Lyrics::Synced(_) | Lyrics::Untimed), "{l:?}");
        // A track nobody has.
        let none = lookup("zzqx nobody", "zzqx nothing at all 91823", "", 0);
        assert_eq!(none, Lyrics::NotFound);
    }
}
