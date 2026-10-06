//! Quick countdown timers ("10 minutes"), on the wall clock like the Pomodoro: a timer stores the
//! second it ends at, so it survives a restart and a sleeping laptop.

use serde::{Deserialize, Serialize};

pub const MAX_TIMERS: usize = 3;
/// Longest timer, in minutes.
pub const MAX_MINUTES: u32 = 600;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timer {
    pub id: u32,
    pub label: String,
    /// UTC second it ends at.
    pub end: i64,
    /// Its length in seconds.
    pub total: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timers {
    items: Vec<Timer>,
    next_id: u32,
}

/// "1 min", "25 min", "1 h", "1 h 30 min".
pub fn label_for(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

impl Timers {
    pub fn items(&self) -> &[Timer] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Start a timer of `minutes`; `None` if the length is out of range or four are already running.
    pub fn add(&mut self, minutes: u32, now: i64) -> Option<u32> {
        if !(1..=MAX_MINUTES).contains(&minutes) || self.items.len() >= MAX_TIMERS {
            return None;
        }
        let id = self.next_id.max(1);
        self.next_id = id.wrapping_add(1).max(1);
        let total = i64::from(minutes) * 60;
        self.items.push(Timer {
            id,
            label: label_for(minutes),
            end: now + total,
            total,
        });
        Some(id)
    }

    pub fn cancel(&mut self, id: u32) -> bool {
        let before = self.items.len();
        self.items.retain(|t| t.id != id);
        self.items.len() != before
    }

    pub fn remaining(&self, id: u32, now: i64) -> i64 {
        self.items
            .iter()
            .find(|t| t.id == id)
            .map_or(0, |t| (t.end - now).max(0))
    }

    /// Remove and return the timers that have run out.
    pub fn take_finished(&mut self, now: i64) -> Vec<Timer> {
        let (done, running): (Vec<Timer>, Vec<Timer>) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|t| t.end <= now);
        self.items = running;
        done
    }

    /// The timer that ends first.
    pub fn soonest(&self) -> Option<&Timer> {
        self.items.iter().min_by_key(|t| t.end)
    }

    /// Repair a list read from disk: bounded, unique ids, sane ends.
    pub fn sanitized(mut self, now: i64) -> Timers {
        self.items.truncate(MAX_TIMERS);
        let mut seen = std::collections::HashSet::new();
        self.items.retain(|t| {
            seen.insert(t.id)
                && t.id != 0
                && (1..=i64::from(MAX_MINUTES) * 60).contains(&t.total)
                && t.end <= now + t.total
        });
        for t in &mut self.items {
            t.label = label_for((t.total / 60).clamp(1, i64::from(MAX_MINUTES)) as u32);
        }
        let max_id = self.items.iter().map(|t| t.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_id.saturating_add(1)).max(1);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(label_for(1), "1 min");
        assert_eq!(label_for(25), "25 min");
        assert_eq!(label_for(60), "1 h");
        assert_eq!(label_for(90), "1 h 30 min");
    }

    #[test]
    fn timers_start_count_down_and_finish_on_the_wall_clock() {
        let mut t = Timers::default();
        let a = t.add(5, 1000).unwrap();
        let b = t.add(1, 1000).unwrap();
        assert_ne!(a, b);
        assert_eq!(t.remaining(a, 1100), 200);
        assert_eq!(t.soonest().map(|x| x.id), Some(b));
        assert!(t.take_finished(1059).is_empty());
        let done = t.take_finished(1060);
        assert_eq!(done.len(), 1);
        assert_eq!((done[0].id, done[0].label.as_str()), (b, "1 min"));
        assert_eq!(t.items().len(), 1);
        assert_eq!(t.remaining(a, 5000), 0, "never negative");
        assert_eq!(t.remaining(999, 0), 0, "unknown id");
    }

    #[test]
    fn lengths_and_counts_are_bounded() {
        let mut t = Timers::default();
        assert_eq!(t.add(0, 0), None);
        assert_eq!(t.add(MAX_MINUTES + 1, 0), None);
        for _ in 0..MAX_TIMERS {
            assert!(t.add(1, 0).is_some());
        }
        assert_eq!(t.add(1, 0), None, "no more than {MAX_TIMERS} at once");
        let id = t.items()[0].id;
        assert!(t.cancel(id));
        assert!(!t.cancel(id));
        assert!(t.add(1, 0).is_some());
    }

    #[test]
    fn json_round_trip_and_repair() {
        let mut t = Timers::default();
        t.add(10, 100).unwrap();
        let json = serde_json::to_string(&t).unwrap();
        let back: Timers = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sanitized(100), t);
        let bad = Timers {
            items: vec![
                Timer {
                    id: 1,
                    label: "x".into(),
                    end: 50,
                    total: 60,
                },
                Timer {
                    id: 1,
                    label: "dup".into(),
                    end: 50,
                    total: 60,
                },
                Timer {
                    id: 0,
                    label: "zero".into(),
                    end: 50,
                    total: 60,
                },
                Timer {
                    id: 2,
                    label: "far".into(),
                    end: i64::MAX,
                    total: 60,
                },
                Timer {
                    id: 3,
                    label: "huge".into(),
                    end: 100,
                    total: 10_i64.pow(12),
                },
            ],
            next_id: 0,
        }
        .sanitized(100);
        assert_eq!(bad.items().len(), 1);
        assert_eq!(
            bad.items()[0].label,
            "1 min",
            "labels are regenerated, not trusted"
        );
        let mut bad = bad;
        let id = bad.add(2, 100).unwrap();
        assert!(bad.items().iter().filter(|t| t.id == id).count() == 1);
        assert_eq!(
            serde_json::from_str::<Timers>("{}").unwrap(),
            Timers::default()
        );
    }
}
