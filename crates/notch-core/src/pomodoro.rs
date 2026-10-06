//! The Pomodoro cycle as a pure state machine.
//!
//! Time is a wall clock in whole UTC seconds supplied by the caller (`Env::unix`): a running timer
//! stores the second it ends at, so it survives the app restarting and a laptop sleeping (a focus
//! session that ran out while the lid was shut is simply finished when the lid opens). Nothing here
//! ticks: the module asks [`Timer::next_change`] when it next needs to look.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Focus,
    ShortBreak,
    LongBreak,
}

impl Phase {
    pub fn is_break(self) -> bool {
        self != Phase::Focus
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Focus => "Focus",
            Phase::ShortBreak => "Short break",
            Phase::LongBreak => "Long break",
        }
    }
}

/// Durations and behaviour, from the configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// Phase lengths in seconds.
    pub focus: i64,
    pub short_break: i64,
    pub long_break: i64,
    /// A long break after this many focus sessions.
    pub long_every: u32,
    /// Start the break by itself when a focus session ends / the focus when a break ends.
    pub auto_breaks: bool,
    pub auto_focus: bool,
}

impl Settings {
    pub fn length(&self, phase: Phase) -> i64 {
        match phase {
            Phase::Focus => self.focus,
            Phase::ShortBreak => self.short_break,
            Phase::LongBreak => self.long_break,
        }
        .max(1)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            focus: 25 * 60,
            short_break: 5 * 60,
            long_break: 15 * 60,
            long_every: 4,
            auto_breaks: true,
            auto_focus: false,
        }
    }
}

/// A phase that just ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finished {
    pub phase: Phase,
    /// The phase that follows (already current).
    pub next: Phase,
    /// The next phase started by itself.
    pub started_next: bool,
    /// A focus session that ran to its end (a skipped one does not count).
    pub focus_done: bool,
}

/// What is persisted of a timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTimer {
    pub phase: Phase,
    /// UTC second the running phase ends, or 0 when stopped.
    pub end: i64,
    pub left: i64,
    pub cycle: u32,
}

#[derive(Clone, Debug)]
pub struct Timer {
    settings: Settings,
    phase: Phase,
    /// `Some(t)`: running, ends at UTC second `t`.
    end: Option<i64>,
    /// Seconds left while stopped.
    left: i64,
    /// Focus sessions finished since the last long break.
    cycle: u32,
}

impl Timer {
    pub fn new(settings: Settings) -> Timer {
        Timer {
            settings,
            phase: Phase::Focus,
            end: None,
            left: settings.length(Phase::Focus),
            cycle: 0,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn running(&self) -> bool {
        self.end.is_some()
    }

    /// Focus sessions finished in the current cycle (0..`long_every`).
    pub fn cycle(&self) -> u32 {
        self.cycle
    }

    /// Whole seconds left (never negative).
    pub fn remaining(&self, now: i64) -> i64 {
        match self.end {
            Some(end) => (end - now).max(0),
            None => self.left,
        }
    }

    /// Length of the current phase.
    pub fn total(&self) -> i64 {
        self.settings.length(self.phase)
    }

    /// 1.0 at the start of a phase down to 0.0 at its end.
    pub fn fraction_left(&self, now: i64) -> f32 {
        (self.remaining(now) as f32 / self.total() as f32).clamp(0.0, 1.0)
    }

    /// Is the stopped timer partway through a phase (paused rather than fresh)?
    pub fn paused(&self) -> bool {
        self.end.is_none() && self.left < self.total()
    }

    pub fn start(&mut self, now: i64) {
        if self.end.is_none() {
            if self.left <= 0 {
                self.left = self.total();
            }
            self.end = Some(now + self.left);
        }
    }

    pub fn pause(&mut self, now: i64) {
        if let Some(end) = self.end.take() {
            self.left = (end - now).clamp(0, self.total());
        }
    }

    pub fn toggle(&mut self, now: i64) {
        if self.running() {
            self.pause(now);
        } else {
            self.start(now);
        }
    }

    /// Back to the start of the current phase, stopped.
    pub fn reset(&mut self) {
        self.end = None;
        self.left = self.total();
    }

    /// Back to a fresh cycle: focus, stopped, no sessions counted.
    pub fn reset_all(&mut self) {
        self.phase = Phase::Focus;
        self.cycle = 0;
        self.reset();
    }

    /// End the current phase now. A skipped focus session does not count as finished.
    pub fn skip(&mut self, now: i64) -> Finished {
        self.finish(now, false)
    }

    /// The moment (UTC second) the running phase ends.
    pub fn next_change(&self) -> Option<i64> {
        self.end
    }

    /// Advance if the running phase has ended. Returns what ended.
    pub fn tick(&mut self, now: i64) -> Option<Finished> {
        match self.end {
            Some(end) if now >= end => Some(self.finish(now, true)),
            _ => None,
        }
    }

    fn finish(&mut self, now: i64, completed: bool) -> Finished {
        let phase = self.phase;
        let focus_done = completed && phase == Phase::Focus;
        let next = match phase {
            Phase::Focus => {
                if focus_done {
                    self.cycle += 1;
                }
                if self.cycle >= self.settings.long_every.max(1) {
                    self.cycle = 0;
                    Phase::LongBreak
                } else {
                    Phase::ShortBreak
                }
            }
            Phase::ShortBreak | Phase::LongBreak => Phase::Focus,
        };
        self.phase = next;
        self.left = self.total();
        let started_next = if next.is_break() {
            self.settings.auto_breaks
        } else {
            self.settings.auto_focus
        };
        // The next phase starts when the user sees the change, not at the (possibly long gone)
        // moment the old one ended.
        self.end = started_next.then_some(now + self.left);
        Finished {
            phase,
            next,
            started_next,
            focus_done,
        }
    }

    /// Adopt new settings. A phase that has not been touched takes the new length at once; a running
    /// or paused one keeps the time it has, but never more than the new length.
    pub fn set_settings(&mut self, settings: Settings, now: i64) {
        let untouched = self.end.is_none() && self.left == self.total();
        self.settings = settings;
        let total = self.total();
        if untouched {
            self.left = total;
        } else {
            self.left = self.left.min(total);
            if let Some(end) = self.end {
                self.end = Some(end.min(now + total));
            }
        }
    }

    pub fn save(&self) -> SavedTimer {
        SavedTimer {
            phase: self.phase,
            end: self.end.unwrap_or(0),
            left: self.left,
            cycle: self.cycle,
        }
    }

    /// Restore a saved timer. A running one whose end is still ahead keeps running; one that ended
    /// while the app was closed comes back stopped at the start of its phase (nothing is awarded for
    /// time nobody was around to see).
    pub fn restore(&mut self, saved: &SavedTimer, now: i64) {
        self.phase = saved.phase;
        self.cycle = saved
            .cycle
            .min(self.settings.long_every.max(1).saturating_sub(1));
        self.left = saved.left.clamp(1, self.total());
        self.end = None;
        if saved.end > now {
            self.end = Some(saved.end.min(now + self.total()));
        } else if saved.end != 0 {
            self.left = self.total();
        }
    }
}

/// `mm:ss` (or `h:mm:ss` from an hour up).
pub fn format_clock(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Minutes shown on the collapsed pill: rounded *up*, so it reads "1m" until the very end.
pub fn chip_minutes(secs: i64) -> i64 {
    (secs.max(0) + 59) / 60
}

/// Seconds until the chip's minute count changes (>= 1), given `rem` seconds left.
pub fn secs_to_chip_change(rem: i64) -> i64 {
    if rem <= 0 {
        return 1;
    }
    let next_boundary = 60 * ((rem - 1) / 60);
    (rem - next_boundary).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick() -> Settings {
        Settings {
            focus: 100,
            short_break: 20,
            long_break: 60,
            long_every: 2,
            auto_breaks: false,
            auto_focus: false,
        }
    }

    #[test]
    fn a_fresh_timer_is_stopped_at_the_start_of_a_focus_session() {
        let t = Timer::new(quick());
        assert_eq!(
            (t.phase(), t.running(), t.remaining(1000)),
            (Phase::Focus, false, 100)
        );
        assert_eq!(t.fraction_left(1000), 1.0);
        assert!(!t.paused());
    }

    #[test]
    fn running_counts_down_on_the_wall_clock() {
        let mut t = Timer::new(quick());
        t.start(1000);
        assert_eq!(t.remaining(1000), 100);
        assert_eq!(t.remaining(1030), 70);
        assert_eq!(t.next_change(), Some(1100));
        assert!((t.fraction_left(1050) - 0.5).abs() < 1e-6);
        assert_eq!(t.remaining(5000), 0, "never negative");
    }

    #[test]
    fn pausing_keeps_the_time_and_resuming_continues_from_it() {
        let mut t = Timer::new(quick());
        t.start(1000);
        t.pause(1040);
        assert!(t.paused() && !t.running());
        assert_eq!(t.remaining(9999), 60, "frozen while paused");
        t.start(2000);
        assert_eq!(t.next_change(), Some(2060));
        t.toggle(2010);
        assert_eq!(t.remaining(0), 50);
        t.toggle(3000);
        assert!(t.running());
    }

    #[test]
    fn a_focus_session_that_ends_leads_to_a_break_and_back() {
        let mut t = Timer::new(quick());
        t.start(0);
        assert_eq!(t.tick(99), None);
        let f = t.tick(100).unwrap();
        assert_eq!(
            f,
            Finished {
                phase: Phase::Focus,
                next: Phase::ShortBreak,
                started_next: false,
                focus_done: true
            }
        );
        assert!(!t.running(), "no auto-start: it waits for the user");
        assert_eq!((t.phase(), t.remaining(0)), (Phase::ShortBreak, 20));
        assert_eq!(t.cycle(), 1);
        t.start(200);
        let f = t.tick(220).unwrap();
        assert_eq!(
            (f.phase, f.next, f.focus_done),
            (Phase::ShortBreak, Phase::Focus, false)
        );
        assert_eq!(t.phase(), Phase::Focus);
    }

    #[test]
    fn every_nth_focus_session_earns_a_long_break_and_resets_the_cycle() {
        let mut t = Timer::new(quick()); // long break after 2
        t.start(0);
        t.tick(100); // focus 1 -> short
        t.start(100);
        t.tick(120); // short -> focus
        t.start(120);
        let f = t.tick(220).unwrap(); // focus 2 -> long
        assert_eq!(f.next, Phase::LongBreak);
        assert_eq!(t.cycle(), 0);
        assert_eq!(t.remaining(0), 60);
    }

    #[test]
    fn auto_start_options_begin_the_next_phase_without_a_click() {
        let mut t = Timer::new(Settings {
            auto_breaks: true,
            ..quick()
        });
        t.start(0);
        let f = t.tick(100).unwrap();
        assert!(f.started_next && t.running());
        assert_eq!(t.next_change(), Some(120));
        let f = t.tick(120).unwrap();
        assert!(!f.started_next, "auto_focus is off: back to focus, stopped");
        let mut t = Timer::new(Settings {
            auto_breaks: true,
            auto_focus: true,
            ..quick()
        });
        t.start(0);
        t.tick(100);
        t.tick(120);
        assert!(t.running() && t.phase() == Phase::Focus);
    }

    #[test]
    fn the_next_phase_starts_when_noticed_not_when_the_old_one_ended() {
        let mut t = Timer::new(Settings {
            auto_breaks: true,
            ..quick()
        });
        t.start(0);
        // The laptop slept through the end of the session and wakes an hour later.
        let f = t.tick(3600).unwrap();
        assert!(f.focus_done && f.started_next);
        assert_eq!(
            t.next_change(),
            Some(3620),
            "the break starts at the wake-up"
        );
        assert_eq!(t.tick(3601), None, "and only one phase is consumed");
    }

    #[test]
    fn skipping_a_focus_session_does_not_count_it() {
        let mut t = Timer::new(quick());
        t.start(0);
        let f = t.skip(30);
        assert!(!f.focus_done);
        assert_eq!((f.phase, f.next), (Phase::Focus, Phase::ShortBreak));
        assert_eq!(t.cycle(), 0);
        let f = t.skip(31);
        assert_eq!((f.phase, f.next), (Phase::ShortBreak, Phase::Focus));
    }

    #[test]
    fn reset_restarts_the_phase_and_reset_all_the_cycle() {
        let mut t = Timer::new(quick());
        t.start(0);
        t.tick(100);
        t.start(100);
        t.pause(110);
        t.reset();
        assert_eq!(
            (t.phase(), t.remaining(0), t.running()),
            (Phase::ShortBreak, 20, false)
        );
        t.reset_all();
        assert_eq!(
            (t.phase(), t.cycle(), t.remaining(0)),
            (Phase::Focus, 0, 100)
        );
    }

    #[test]
    fn new_settings_apply_to_an_untouched_phase_but_not_to_a_running_one() {
        let with_focus = |focus| Settings { focus, ..quick() };
        let mut t = Timer::new(quick());
        t.set_settings(with_focus(200), 0);
        assert_eq!(t.remaining(0), 200);
        t.start(0);
        t.set_settings(with_focus(50), 10);
        assert_eq!(
            t.remaining(10),
            50,
            "a running session keeps its end, capped by the new length"
        );
        t.set_settings(with_focus(500), 20);
        assert_eq!(t.remaining(20), 40, "a longer length does not extend it");
        let mut t = Timer::new(quick());
        t.start(0);
        t.pause(10);
        t.set_settings(with_focus(300), 10);
        assert_eq!(t.remaining(0), 90, "a paused session keeps the time it has");
    }

    #[test]
    fn save_and_restore_round_trip_a_running_timer() {
        let mut t = Timer::new(quick());
        t.start(1000);
        let saved = t.save();
        let json = serde_json::to_string(&saved).unwrap();
        let back: SavedTimer = serde_json::from_str(&json).unwrap();
        let mut u = Timer::new(quick());
        u.restore(&back, 1030);
        assert!(u.running());
        assert_eq!(u.remaining(1030), 70);
    }

    #[test]
    fn a_timer_that_ended_while_the_app_was_closed_comes_back_stopped() {
        let mut t = Timer::new(quick());
        t.start(1000);
        let saved = t.save();
        let mut u = Timer::new(quick());
        u.restore(&saved, 5000);
        assert!(!u.running());
        assert_eq!(u.remaining(5000), 100, "restarts the phase, awards nothing");
    }

    #[test]
    fn restore_distrusts_nonsense() {
        let mut u = Timer::new(quick());
        u.restore(
            &SavedTimer {
                phase: Phase::Focus,
                end: i64::MAX,
                left: -5,
                cycle: 99,
            },
            1000,
        );
        assert!(u.running());
        assert!(
            u.remaining(1000) <= 100,
            "an absurd end is capped to one phase"
        );
        assert!(u.cycle() < 2);
        let mut v = Timer::new(quick());
        v.restore(
            &SavedTimer {
                phase: Phase::ShortBreak,
                end: 0,
                left: 99_999,
                cycle: 0,
            },
            0,
        );
        assert_eq!(v.remaining(0), 20, "left is clamped to the phase length");
    }

    #[test]
    fn clock_and_chip_formatting() {
        assert_eq!(format_clock(0), "00:00");
        assert_eq!(format_clock(1500), "25:00");
        assert_eq!(format_clock(59), "00:59");
        assert_eq!(format_clock(3725), "1:02:05");
        assert_eq!(format_clock(-4), "00:00");
        assert_eq!(chip_minutes(1500), 25);
        assert_eq!(chip_minutes(1441), 25);
        assert_eq!(chip_minutes(1440), 24);
        assert_eq!(chip_minutes(1), 1);
        assert_eq!(chip_minutes(0), 0);
    }

    #[test]
    fn the_chip_changes_exactly_when_its_minute_count_does() {
        for rem in 1..=300i64 {
            let wait = secs_to_chip_change(rem);
            assert!(wait >= 1);
            let before = chip_minutes(rem);
            let after = chip_minutes(rem - wait);
            if rem - wait > 0 {
                assert_eq!(
                    after,
                    before - 1,
                    "rem {rem}: {before} -> {after} after {wait}s"
                );
            }
            // And nothing changes earlier than that.
            for d in 1..wait {
                assert_eq!(
                    chip_minutes(rem - d),
                    before,
                    "rem {rem} changed early at +{d}"
                );
            }
        }
        assert_eq!(secs_to_chip_change(60), 60, "'1m' holds until the end");
        assert_eq!(secs_to_chip_change(61), 1);
        assert_eq!(secs_to_chip_change(1500), 60);
    }
}
