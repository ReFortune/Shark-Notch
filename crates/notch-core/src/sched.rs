//! A tiny deadline scheduler. The UI loop asks [`Scheduler::next_deadline`] and uses it as the
//! wait timeout, so there are no OS timers at all and nothing wakes the process while no deadline is
//! pending.

/// Identifier of a scheduled timer. Each subsystem uses its own constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerId(pub u32);

#[derive(Debug, Default)]
pub struct Scheduler {
    timers: Vec<(TimerId, f64)>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule (or move) a timer to fire at monotonic time `at` (seconds).
    pub fn set(&mut self, id: TimerId, at: f64) {
        if let Some(t) = self.timers.iter_mut().find(|t| t.0 == id) {
            t.1 = at;
        } else {
            self.timers.push((id, at));
        }
    }

    /// Schedule only if it would fire earlier than the existing deadline (or none exists).
    pub fn set_earliest(&mut self, id: TimerId, at: f64) {
        match self.timers.iter_mut().find(|t| t.0 == id) {
            Some(t) => t.1 = t.1.min(at),
            None => self.timers.push((id, at)),
        }
    }

    pub fn cancel(&mut self, id: TimerId) {
        self.timers.retain(|t| t.0 != id);
    }

    pub fn is_pending(&self, id: TimerId) -> bool {
        self.timers.iter().any(|t| t.0 == id)
    }

    pub fn deadline(&self, id: TimerId) -> Option<f64> {
        self.timers.iter().find(|t| t.0 == id).map(|t| t.1)
    }

    pub fn next_deadline(&self) -> Option<f64> {
        self.timers
            .iter()
            .map(|t| t.1)
            .min_by(|a, b| a.total_cmp(b))
    }

    /// Pop every timer due at `now`, in deadline order, into `out`.
    pub fn take_due(&mut self, now: f64, out: &mut Vec<TimerId>) {
        out.clear();
        let mut due: Vec<(TimerId, f64)> =
            self.timers.iter().copied().filter(|t| t.1 <= now).collect();
        if due.is_empty() {
            return;
        }
        due.sort_by(|a, b| a.1.total_cmp(&b.1));
        self.timers.retain(|t| t.1 > now);
        out.extend(due.into_iter().map(|t| t.0));
    }

    pub fn clear(&mut self) {
        self.timers.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.timers.is_empty()
    }

    /// Convert a deadline into a wait timeout in whole milliseconds, rounded up so the loop never
    /// wakes early and spins. `None` means wait forever.
    pub fn timeout_ms(&self, now: f64) -> Option<u32> {
        self.next_deadline().map(|d| {
            let ms = ((d - now) * 1000.0).ceil();
            if ms <= 0.0 {
                0
            } else {
                ms.min(u32::MAX as f64 - 1.0) as u32
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: TimerId = TimerId(1);
    const B: TimerId = TimerId(2);
    const C: TimerId = TimerId(3);

    #[test]
    fn fires_in_order_once() {
        let mut s = Scheduler::new();
        s.set(B, 2.0);
        s.set(A, 1.0);
        s.set(C, 5.0);
        let mut due = Vec::new();
        s.take_due(0.5, &mut due);
        assert!(due.is_empty());
        s.take_due(2.5, &mut due);
        assert_eq!(due, vec![A, B]);
        s.take_due(2.5, &mut due);
        assert!(due.is_empty(), "fired timers are removed");
        assert_eq!(s.next_deadline(), Some(5.0));
    }

    #[test]
    fn set_moves_and_set_earliest_only_pulls_in() {
        let mut s = Scheduler::new();
        s.set(A, 10.0);
        s.set(A, 4.0);
        assert_eq!(s.deadline(A), Some(4.0));
        s.set_earliest(A, 9.0);
        assert_eq!(s.deadline(A), Some(4.0));
        s.set_earliest(A, 2.0);
        assert_eq!(s.deadline(A), Some(2.0));
        s.set_earliest(B, 7.0);
        assert!(s.is_pending(B));
        s.cancel(A);
        assert!(!s.is_pending(A));
    }

    #[test]
    fn timeout_rounds_up_and_none_when_idle() {
        let mut s = Scheduler::new();
        assert_eq!(s.timeout_ms(0.0), None, "no deadline means wait forever");
        s.set(A, 1.0004);
        assert_eq!(
            s.timeout_ms(1.0),
            Some(1),
            "0.4 ms rounds up to 1 ms, never spins"
        );
        assert_eq!(s.timeout_ms(2.0), Some(0), "overdue fires immediately");
    }
}
