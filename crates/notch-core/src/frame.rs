//! Frame pacing maths and the frame-time recorder.
//!
//! The animation clock is the *compositor's* vblank timeline, not "now". Wake-ups from a waitable
//! object jitter by a fraction of a millisecond; at 3000 px/s that is ~1.5 px of visible judder if the
//! spring is sampled at the wake-up time. Sampling at the (predicted) display time removes it.

/// Predict the display time of the frame being prepared at `now`, given a known past vblank and the
/// refresh `period` (all in seconds on one monotonic clock). Falls back to `now`.
///
/// The frame is assumed to be presented at the first vblank at or after `now`.
pub fn sample_time(now: f64, last_vblank: Option<f64>, period: f64) -> f64 {
    match last_vblank {
        Some(v) if period > 1e-4 && period < 0.1 => {
            let n = ((now - v) / period).ceil();
            let t = v + n * period;
            // Guard against a stale/garbage vblank far from `now`.
            if (t - now).abs() < 2.0 * period {
                t
            } else {
                now
            }
        }
        _ => now,
    }
}

/// One animation burst's statistics.
#[derive(Clone, Debug, PartialEq)]
pub struct BurstReport {
    pub label: String,
    pub frames: usize,
    pub duration_ms: f64,
    pub period_ms: f64,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Intervals longer than 1.5x the refresh period.
    pub hitches: usize,
    /// Of those, the frames whose own building took more than half a refresh period: the app's
    /// drawing was slow. The others were late for a reason outside the frame's own cost (the
    /// compositor or display did not hand over the next slot in time, or the whole machine stalled).
    pub app_hitches: usize,
    pub worst_cpu_ms: f64,
    pub avg_cpu_ms: f64,
}

impl BurstReport {
    pub fn has_hitch(&self) -> bool {
        self.hitches > 0
    }

    pub fn format(&self) -> String {
        format!(
            "burst '{}': {} frames over {:.0} ms (refresh {:.2} ms) interval avg {:.2} p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms | cpu avg {:.2} worst {:.2} ms | {}",
            self.label,
            self.frames,
            self.duration_ms,
            self.period_ms,
            self.avg_ms,
            self.p50_ms,
            self.p95_ms,
            self.p99_ms,
            self.max_ms,
            self.avg_cpu_ms,
            self.worst_cpu_ms,
            if self.hitches == 0 {
                "OK".to_string()
            } else {
                format!(
                    "HITCH x{} (slow drawing {}, late presentation {})",
                    self.hitches,
                    self.app_hitches,
                    self.hitches - self.app_hitches
                )
            },
        )
    }
}

/// Counts over **every** burst since the recorder was made. The stored reports are bounded (the
/// last few), these are not: a long session's hitches are never dropped from the total just because
/// their burst has scrolled out of the list.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub bursts: usize,
    pub frames: usize,
    pub hitches: usize,
    /// Of the hitches, the frames that were themselves slow to build (see
    /// [`BurstReport::app_hitches`]).
    pub app_hitches: usize,
    /// The longest interval between two presented frames, in milliseconds.
    pub worst_interval_ms: f64,
    /// The longest a single frame took to build, in milliseconds.
    pub worst_cpu_ms: f64,
}

/// Records frame intervals and per-frame CPU cost for the current burst and keeps the last few
/// finished reports. Pre-allocated: `frame()` never allocates after the first burst.
#[derive(Debug)]
pub struct FrameRecorder {
    label: String,
    active: bool,
    last: Option<f64>,
    first: f64,
    intervals: Vec<f32>,
    cpu: Vec<f32>,
    period: f64,
    done: Vec<BurstReport>,
    max_reports: usize,
    totals: Totals,
}

impl Default for FrameRecorder {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_REPORTS: usize = 16;
const MAX_FRAMES: usize = 4096;

impl FrameRecorder {
    pub fn new() -> Self {
        Self {
            label: String::new(),
            active: false,
            last: None,
            first: 0.0,
            intervals: Vec::with_capacity(512),
            cpu: Vec::with_capacity(512),
            period: 0.0,
            done: Vec::new(),
            max_reports: MAX_REPORTS,
            totals: Totals::default(),
        }
    }

    /// Keep this many finished reports instead of the default few (the self-test keeps them all so
    /// its report can list every burst). The [`Totals`] never depend on this.
    pub fn set_max_reports(&mut self, n: usize) {
        self.max_reports = n.max(1);
    }

    /// Counts over every burst so far.
    pub fn totals(&self) -> Totals {
        self.totals
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Number of frame intervals recorded in the current burst.
    pub fn len(&self) -> usize {
        self.intervals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    /// Begin a burst. `period` is the current refresh period in seconds.
    pub fn begin(&mut self, label: &str, now: f64, period: f64) {
        if self.active {
            self.finish();
        }
        self.active = true;
        self.label.clear();
        self.label.push_str(label);
        self.last = None;
        self.first = now;
        self.period = period;
        self.intervals.clear();
        self.cpu.clear();
    }

    /// Record a presented frame at time `now` that cost `cpu_ms` of CPU to build.
    pub fn frame(&mut self, now: f64, cpu_ms: f32) {
        if !self.active {
            return;
        }
        if let Some(prev) = self.last
            && self.intervals.len() < MAX_FRAMES
        {
            self.intervals.push(((now - prev) * 1000.0) as f32);
            self.cpu.push(cpu_ms);
        }
        self.last = Some(now);
    }

    /// Finish the burst; returns the report (also stored).
    pub fn finish(&mut self) -> Option<BurstReport> {
        if !self.active {
            return None;
        }
        self.active = false;
        let end = self.last.unwrap_or(self.first);
        if self.intervals.is_empty() {
            return None;
        }
        let mut sorted = self.intervals.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let pct = |p: f64| sorted[(((sorted.len() - 1) as f64) * p).round() as usize] as f64;
        let period_ms = self.period * 1000.0;
        let (hitches, app_hitches) = if period_ms > 0.0 {
            self.intervals
                .iter()
                .zip(&self.cpu)
                .filter(|&(&i, _)| f64::from(i) > 1.5 * period_ms)
                .fold((0, 0), |(all, app), (_, &cpu)| {
                    (all + 1, app + usize::from(f64::from(cpu) > 0.5 * period_ms))
                })
        } else {
            (0, 0)
        };
        let rep = BurstReport {
            label: self.label.clone(),
            frames: self.intervals.len() + 1,
            duration_ms: (end - self.first) * 1000.0,
            period_ms,
            avg_ms: self.intervals.iter().map(|&i| f64::from(i)).sum::<f64>()
                / self.intervals.len() as f64,
            p50_ms: pct(0.50),
            p95_ms: pct(0.95),
            p99_ms: pct(0.99),
            max_ms: *sorted.last().unwrap() as f64,
            hitches,
            app_hitches,
            worst_cpu_ms: self.cpu.iter().cloned().fold(0.0f32, f32::max) as f64,
            avg_cpu_ms: self.cpu.iter().map(|&c| f64::from(c)).sum::<f64>() / self.cpu.len() as f64,
        };
        let t = &mut self.totals;
        t.bursts += 1;
        t.frames += rep.frames;
        t.hitches += rep.hitches;
        t.app_hitches += rep.app_hitches;
        t.worst_interval_ms = t.worst_interval_ms.max(rep.max_ms);
        t.worst_cpu_ms = t.worst_cpu_ms.max(rep.worst_cpu_ms);
        while self.done.len() >= self.max_reports {
            self.done.remove(0);
        }
        self.done.push(rep.clone());
        Some(rep)
    }

    pub fn reports(&self) -> &[BurstReport] {
        &self.done
    }

    pub fn format_all(&self) -> String {
        if self.done.is_empty() {
            return "no animation bursts recorded yet".to_string();
        }
        self.done
            .iter()
            .map(BurstReport::format)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Hitches in every burst so far, not only the stored ones.
    pub fn total_hitches(&self) -> usize {
        self.totals.hitches
    }

    /// Hitches whose frame was itself slow to build (see [`BurstReport::app_hitches`]).
    pub fn total_app_hitches(&self) -> usize {
        self.totals.app_hitches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_time_snaps_to_the_next_vblank() {
        let period = 1.0 / 60.0;
        // Wake 0.5 ms after a vblank: the frame will be shown at the *next* one.
        let t = sample_time(10.0005, Some(10.0), period);
        assert!((t - (10.0 + period)).abs() < 1e-9, "{t}");
        // Jittered wake-ups within one refresh map to the same display time (that is the point).
        let a = sample_time(10.001, Some(10.0), period);
        let b = sample_time(10.012, Some(10.0), period);
        assert!((a - b).abs() < 1e-9);
        // A later vblank reference gives the same grid.
        let c = sample_time(10.0005 + 5.0 * period, Some(10.0 + 3.0 * period), period);
        assert!((c - (10.0 + 6.0 * period)).abs() < 1e-9);
    }

    #[test]
    fn sample_time_survives_garbage() {
        assert_eq!(sample_time(5.0, None, 1.0 / 60.0), 5.0);
        assert_eq!(
            sample_time(5.0, Some(1.0), 1.0 / 60.0),
            5.0,
            "stale vblank ignored"
        );
        assert_eq!(sample_time(5.0, Some(5.0), 0.0), 5.0, "zero period ignored");
        assert_eq!(
            sample_time(5.0, Some(5.0), 5.0),
            5.0,
            "absurd period ignored"
        );
    }

    #[test]
    fn recorder_reports_percentiles_and_hitches() {
        let period = 1.0 / 60.0;
        let mut r = FrameRecorder::new();
        r.begin("expand", 0.0, period);
        let mut t = 0.0;
        for i in 0..100 {
            t += if i == 50 { period * 2.0 } else { period }; // one dropped frame
            r.frame(t, 1.5);
        }
        let rep = r.finish().unwrap();
        assert_eq!(rep.frames, 100);
        assert_eq!(rep.hitches, 1);
        assert!(rep.has_hitch());
        assert!((rep.p50_ms - 16.67).abs() < 0.1);
        assert!((rep.max_ms - 33.33).abs() < 0.1);
        assert!((rep.avg_cpu_ms - 1.5).abs() < 1e-3);
        assert!(rep.format().contains("HITCH x1"));
        assert_eq!(r.total_hitches(), 1);
        // The late frame cost 1.5 ms to build: the machine was late, not the drawing.
        assert_eq!(rep.app_hitches, 0);
        assert!(rep.format().contains("slow drawing 0, late presentation 1"));
        assert_eq!(r.total_app_hitches(), 0);
    }

    #[test]
    fn a_late_frame_that_was_slow_to_draw_is_told_apart_from_a_late_present() {
        let period = 1.0 / 60.0; // 16.7 ms: "slow" means more than 8.3 ms of drawing
        let mut r = FrameRecorder::new();
        r.begin("expand", 0.0, period);
        let mut t = 0.0;
        // Frame 1 is late and took 120 ms to draw; frame 2 is late with 1 ms of drawing; frame 3
        // is on time but took 12 ms (slow, yet it did not make anything late); the rest are fine.
        for (gap, cpu) in [
            (period, 1.0),
            (period * 7.0, 120.0),
            (period * 2.0, 1.0),
            (period, 12.0),
            (period, 1.0),
        ] {
            t += gap;
            r.frame(t, cpu);
        }
        let rep = r.finish().unwrap();
        assert_eq!((rep.hitches, rep.app_hitches), (2, 1));
        assert!(
            rep.format()
                .contains("HITCH x2 (slow drawing 1, late presentation 1)")
        );
        assert_eq!(r.total_hitches(), 2);
        assert_eq!(r.total_app_hitches(), 1);
    }

    #[test]
    fn the_totals_count_every_burst_even_after_the_stored_reports_scrolled_away() {
        let period = 1.0 / 60.0;
        let mut r = FrameRecorder::new();
        r.set_max_reports(3);
        for b in 0..10 {
            r.begin(&format!("b{b}"), 0.0, period);
            // Four frames, one of them late and slow to draw (30 ms).
            for (gap, cpu) in [
                (period, 1.0),
                (period * 3.0, 30.0),
                (period, 1.0),
                (period, 1.0),
            ] {
                let t = r.last.unwrap_or(0.0) + gap;
                r.frame(t, cpu);
            }
            r.finish().unwrap();
        }
        assert_eq!(r.reports().len(), 3, "only the last three are kept");
        assert_eq!(r.reports().first().unwrap().label, "b7");
        let t = r.totals();
        assert_eq!((t.bursts, t.hitches, t.app_hitches), (10, 10, 10));
        assert_eq!(t.frames, 40);
        assert!((t.worst_interval_ms - 50.0).abs() < 0.1, "{t:?}");
        assert!((t.worst_cpu_ms - 30.0).abs() < 1e-3);
        assert_eq!((r.total_hitches(), r.total_app_hitches()), (10, 10));
        // A recorder asked to keep everything does.
        let mut all = FrameRecorder::new();
        all.set_max_reports(1000);
        for b in 0..40 {
            all.begin(&format!("b{b}"), 0.0, period);
            all.frame(period, 1.0);
            all.frame(period * 2.0, 1.0);
            all.finish().unwrap();
        }
        assert_eq!(all.reports().len(), 40);
        assert_eq!(FrameRecorder::default().totals(), Totals::default());
    }

    #[test]
    fn clean_burst_is_ok_and_reports_are_bounded() {
        let mut r = FrameRecorder::new();
        for b in 0..40 {
            r.begin(&format!("b{b}"), 0.0, 1.0 / 144.0);
            for i in 1..30 {
                r.frame(i as f64 / 144.0, 0.4);
            }
            let rep = r.finish().unwrap();
            assert!(!rep.has_hitch());
            assert!(rep.format().ends_with("OK"));
        }
        assert_eq!(r.reports().len(), MAX_REPORTS);
        assert_eq!(r.reports().last().unwrap().label, "b39");
    }

    #[test]
    fn empty_burst_yields_nothing() {
        let mut r = FrameRecorder::new();
        assert!(r.finish().is_none());
        r.begin("x", 0.0, 1.0 / 60.0);
        r.frame(0.0, 1.0); // a single frame has no interval
        assert!(r.finish().is_none());
        assert_eq!(r.format_all(), "no animation bursts recorded yet");
    }
}
