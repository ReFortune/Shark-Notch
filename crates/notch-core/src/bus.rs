//! The event bus: any thread can [`BusSender::send`]; the UI thread drains with [`Bus::drain`].
//!
//! * Producers never block and never touch UI state.
//! * Waking the UI thread is **coalesced**: at most one wake-up is outstanding no matter how many
//!   events arrive before the UI thread gets around to draining (a burst of 1,000 clipboard events
//!   posts one message, not 1,000).
//! * Dispatch to subscribers happens on the UI thread (see `module::ModuleHost`), so modules need no
//!   locking and can hold non-`Send` state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::events::{Event, EventKind, Source};

/// Wakes the UI thread (on Windows: `PostMessageW`). Called from producer threads.
pub trait Waker: Send + Sync {
    fn wake(&self);
}

#[derive(Clone)]
pub struct BusSender {
    tx: Sender<Event>,
    waker: Arc<dyn Waker>,
    pending: Arc<AtomicBool>,
}

impl BusSender {
    pub fn send(&self, source: Source, kind: EventKind) {
        self.send_event(Event::new(source, kind));
    }

    pub fn send_event(&self, ev: Event) {
        // If the receiver is gone the app is shutting down; dropping the event is correct.
        if self.tx.send(ev).is_ok() && !self.pending.swap(true, Ordering::SeqCst) {
            self.waker.wake();
        }
    }
}

pub struct Bus {
    rx: Receiver<Event>,
    pending: Arc<AtomicBool>,
}

impl Bus {
    pub fn new(waker: Arc<dyn Waker>) -> (Bus, BusSender) {
        let (tx, rx) = channel();
        let pending = Arc::new(AtomicBool::new(false));
        (
            Bus {
                rx,
                pending: pending.clone(),
            },
            BusSender { tx, waker, pending },
        )
    }

    /// Take everything queued so far (in arrival order) into `out`. Clears the wake-up flag *first*,
    /// so an event arriving while we drain schedules a fresh wake-up instead of being missed.
    pub fn drain(&mut self, out: &mut Vec<Event>) {
        self.pending.store(false, Ordering::SeqCst);
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::BatteryInfo;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct CountingWaker(AtomicUsize);
    impl Waker for CountingWaker {
        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn battery(p: u8) -> EventKind {
        EventKind::Battery(BatteryInfo {
            percent: p,
            charging: false,
        })
    }

    #[test]
    fn events_arrive_in_order_with_their_source() {
        let w = Arc::new(CountingWaker::default());
        let (mut bus, tx) = Bus::new(w);
        tx.send(Source::Local, battery(10));
        tx.send(Source::Phone, battery(20));
        tx.send(Source::Local, battery(30));
        let mut out = Vec::new();
        bus.drain(&mut out);
        let seen: Vec<(Source, u8)> = out
            .iter()
            .map(|e| match &e.kind {
                EventKind::Battery(b) => (e.source, b.percent),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                (Source::Local, 10),
                (Source::Phone, 20),
                (Source::Local, 30)
            ]
        );
    }

    #[test]
    fn wakeups_are_coalesced_until_drained() {
        let w = Arc::new(CountingWaker::default());
        let (mut bus, tx) = Bus::new(w.clone());
        for i in 0..1000 {
            tx.send(Source::Local, battery((i % 100) as u8));
        }
        assert_eq!(w.0.load(Ordering::SeqCst), 1, "1000 events, one wake-up");
        let mut out = Vec::new();
        bus.drain(&mut out);
        assert_eq!(out.len(), 1000);
        // After draining, the next event wakes again.
        tx.send(Source::Phone, battery(1));
        assert_eq!(w.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn producers_on_many_threads() {
        let w = Arc::new(CountingWaker::default());
        let (mut bus, tx) = Bus::new(w);
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for i in 0..250u32 {
                        tx.send(
                            if t % 2 == 0 {
                                Source::Local
                            } else {
                                Source::Phone
                            },
                            battery(((t * 7 + i) % 100) as u8),
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let mut out = Vec::new();
        bus.drain(&mut out);
        assert_eq!(out.len(), 2000, "nothing lost under contention");
        assert_eq!(
            out.iter().filter(|e| e.source == Source::Phone).count(),
            1000
        );
    }

    #[test]
    fn sending_after_the_receiver_is_gone_is_harmless() {
        let w = Arc::new(CountingWaker::default());
        let (bus, tx) = Bus::new(w.clone());
        drop(bus);
        tx.send(Source::Local, battery(1));
        assert_eq!(w.0.load(Ordering::SeqCst), 0, "no wake-up for a dead bus");
    }

    #[test]
    fn an_event_arriving_during_drain_is_not_lost() {
        let w = Arc::new(CountingWaker::default());
        let (mut bus, tx) = Bus::new(w.clone());
        tx.send(Source::Local, battery(1));
        let mut out = Vec::new();
        bus.drain(&mut out);
        tx.send(Source::Local, battery(2)); // arrives right after the drain cleared the flag
        assert_eq!(w.0.load(Ordering::SeqCst), 2);
        bus.drain(&mut out);
        assert_eq!(out.len(), 2);
    }
}
