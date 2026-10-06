//! System statistics: the arithmetic and formatting around what the platform samples.
//!
//! The platform reads raw counters (CPU times, network byte counts, GPU engine utilisation). Turning
//! two readings into a percentage or a rate, and the numbers into text, needs no operating system,
//! so it lives here where it is tested.

use std::collections::HashMap;

use crate::events::{BatteryInfo, PowerStatus};

/// Processor time counters, in any unit (only differences matter). `kernel` includes `idle`, as
/// Windows' `GetSystemTimes` reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    pub kernel: u64,
    pub user: u64,
}

/// Busy share of the processor between two readings, 0..=100. `None` if no time passed or the
/// counters went backwards.
pub fn cpu_percent(prev: CpuTimes, cur: CpuTimes) -> Option<f32> {
    let total =
        (cur.kernel.checked_sub(prev.kernel)?).checked_add(cur.user.checked_sub(prev.user)?)?;
    let idle = cur.idle.checked_sub(prev.idle)?;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle);
    Some((busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32)
}

/// Bytes received and sent, summed over the physical network adapters that are up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetCounters {
    pub rx: u64,
    pub tx: u64,
}

/// `(download, upload)` in bytes per second between two readings `secs` apart. A counter that went
/// backwards (an adapter was removed or reset, so the sum dropped) reads as 0 for that reading
/// rather than as a huge wrapped-around spike. `None` if no time passed.
pub fn net_rate(prev: NetCounters, cur: NetCounters, secs: f64) -> Option<(f64, f64)> {
    if secs <= 0.0 || !secs.is_finite() {
        return None;
    }
    let rate = |a: u64, b: u64| b.saturating_sub(a) as f64 / secs;
    Some((rate(prev.rx, cur.rx), rate(prev.tx, cur.tx)))
}

/// GPU utilisation 0..=100 from Windows' per-process "GPU Engine" performance counters: one value
/// per instance, named like `pid_1234_luid_0x00000000_0x0000E0A1_phys_0_eng_0_engtype_3D`.
///
/// Instances of the same engine (same adapter, same engine number and type) are summed across
/// processes — that is how busy that engine is — and the busiest engine is the answer, which is
/// what Task Manager's headline GPU figure does. `None` if there are no instances at all (the
/// counters do not exist on this machine).
pub fn gpu_percent(instances: &[(String, f64)]) -> Option<f32> {
    if instances.is_empty() {
        return None;
    }
    let mut engines: HashMap<&str, f64> = HashMap::new();
    for (name, value) in instances {
        // Everything after the process id identifies the engine.
        let engine = name.find("_luid_").map_or(name.as_str(), |i| &name[i..]);
        let v = if value.is_finite() {
            value.max(0.0)
        } else {
            0.0
        };
        *engines.entry(engine).or_insert(0.0) += v;
    }
    let busiest = engines.values().copied().fold(0.0f64, f64::max);
    Some(busiest.clamp(0.0, 100.0) as f32)
}

/// Windows' `SYSTEM_POWER_STATUS` read as a [`PowerStatus`]. `None` for a PC without a battery (flag
/// bit 128) or when the battery state is unknown (flag or percentage 255).
///
/// * `ac_line`: 0 on battery, 1 on mains, 255 unknown.
/// * `flag`: bit 8 = charging, bit 128 = no battery, 255 = unknown.
/// * `percent`: 0..=100, 255 unknown.
/// * `status`: bit 1 = battery saver on.
/// * `life_secs`: seconds of battery left, `u32::MAX` unknown (it is also not reported while on
///   mains power).
pub fn power_from_raw(
    ac_line: u8,
    flag: u8,
    percent: u8,
    status: u8,
    life_secs: u32,
) -> Option<PowerStatus> {
    if flag == 255 || flag & 128 != 0 || percent > 100 {
        return None;
    }
    let plugged = ac_line == 1;
    Some(PowerStatus {
        battery: BatteryInfo {
            percent,
            charging: flag & 8 != 0,
        },
        plugged,
        secs_left: (!plugged && life_secs != u32::MAX).then_some(life_secs),
        saver: status & 1 != 0,
    })
}

/// The most recent values of one measurement, oldest first, bounded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    values: Vec<f32>,
    cap: usize,
}

impl History {
    pub fn new(cap: usize) -> History {
        History {
            values: Vec::with_capacity(cap.max(1)),
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, v: f32) {
        if self.values.len() == self.cap {
            self.values.remove(0);
        }
        self.values.push(v);
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    pub fn values(&self) -> &[f32] {
        &self.values
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn max(&self) -> f32 {
        self.values.iter().copied().fold(0.0, f32::max)
    }
}

/// "812 B/s", "4.2 KB/s", "12 MB/s", or as bits ("850 Kbps", "9.8 Mbps", "1.2 Gbps": decimal
/// multiples, as network speeds are quoted).
pub fn fmt_rate(bytes_per_sec: f64, bits: bool) -> String {
    let b = bytes_per_sec.max(0.0);
    if bits {
        let v = b * 8.0;
        return if v < 1_000.0 {
            format!("{v:.0} bps")
        } else if v < 1_000_000.0 {
            format!("{:.0} Kbps", v / 1_000.0)
        } else if v < 1_000_000_000.0 {
            scaled(v / 1_000_000.0, "Mbps")
        } else {
            scaled(v / 1_000_000_000.0, "Gbps")
        };
    }
    const K: f64 = 1024.0;
    if b < K {
        format!("{b:.0} B/s")
    } else if b < K * K {
        scaled(b / K, "KB/s")
    } else if b < K * K * K {
        scaled(b / (K * K), "MB/s")
    } else {
        scaled(b / (K * K * K), "GB/s")
    }
}

/// One decimal below 10, none above ("4.2", "12").
fn scaled(v: f64, unit: &str) -> String {
    if v < 10.0 {
        format!("{v:.1} {unit}")
    } else {
        format!("{v:.0} {unit}")
    }
}

/// "15.8 GB", "512 MB": memory sizes the way Windows words them (binary multiples).
pub fn fmt_memory(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let x = bytes as f64;
    if x < 1024.0 * MB {
        format!("{:.0} MB", x / MB)
    } else {
        format!("{:.1} GB", x / (1024.0 * MB))
    }
}

/// "2 h 14 min", "45 min", "under a minute".
pub fn fmt_remaining(secs: u32) -> String {
    let mins = secs / 60;
    match (mins / 60, mins % 60) {
        (0, 0) => "under a minute".to_string(),
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(idle: u64, kernel: u64, user: u64) -> CpuTimes {
        CpuTimes { idle, kernel, user }
    }

    #[test]
    fn cpu_is_the_busy_share_of_the_elapsed_processor_time() {
        // 1000 ticks passed on every core in total; 750 of them idle (kernel includes idle).
        let a = t(10_000, 12_000, 3_000);
        let b = t(10_750, 12_600, 3_400);
        assert!((cpu_percent(a, b).unwrap() - 25.0).abs() < 1e-4);
        assert_eq!(
            cpu_percent(a, t(10_000, 12_000, 3_000)),
            None,
            "no time passed"
        );
        assert_eq!(cpu_percent(b, a), None, "counters went backwards");
        let busy = cpu_percent(a, t(10_000, 12_500, 3_500)).unwrap();
        assert!((busy - 100.0).abs() < 1e-4, "{busy}");
        // More idle than total (rounding in the kernel's counters) never goes negative.
        assert_eq!(cpu_percent(a, t(11_100, 12_900, 3_100)), Some(0.0));
    }

    #[test]
    fn network_rates_are_per_second_and_survive_a_vanished_adapter() {
        let a = NetCounters { rx: 1_000, tx: 500 };
        let b = NetCounters {
            rx: 3_048,
            tx: 1_524,
        };
        let (down, up) = net_rate(a, b, 2.0).unwrap();
        assert!((down - 1_024.0).abs() < 1e-9 && (up - 512.0).abs() < 1e-9);
        // The sum dropped (an adapter went away): zero, not a wrapped-around spike.
        assert_eq!(net_rate(b, a, 1.0), Some((0.0, 0.0)));
        assert_eq!(net_rate(a, b, 0.0), None);
        assert_eq!(net_rate(a, b, f64::NAN), None);
        assert_eq!(net_rate(a, b, -1.0), None);
    }

    fn inst(name: &str, v: f64) -> (String, f64) {
        (name.to_string(), v)
    }

    #[test]
    fn gpu_is_the_busiest_engine_summed_over_processes() {
        let list = [
            inst("pid_100_luid_0x0_0xA_phys_0_eng_0_engtype_3D", 30.0),
            inst("pid_200_luid_0x0_0xA_phys_0_eng_0_engtype_3D", 25.0),
            inst("pid_100_luid_0x0_0xA_phys_0_eng_1_engtype_Copy", 10.0),
            inst("pid_300_luid_0x0_0xB_phys_0_eng_0_engtype_3D", 5.0),
        ];
        // Engine 3D of adapter A: 30 + 25 = 55, the busiest.
        assert!((gpu_percent(&list).unwrap() - 55.0).abs() < 1e-4);
        assert_eq!(gpu_percent(&[]), None, "no counters on this machine");
        // All idle is 0, not "unavailable".
        assert_eq!(
            gpu_percent(&[inst("pid_1_luid_0x0_0xA_phys_0_eng_0_engtype_3D", 0.0)]),
            Some(0.0)
        );
        // Over-subscription, junk and negatives are clamped.
        let wild = [
            inst("pid_1_luid_0x0_0xA_phys_0_eng_0_engtype_3D", 90.0),
            inst("pid_2_luid_0x0_0xA_phys_0_eng_0_engtype_3D", 90.0),
            inst("pid_3_luid_0x0_0xA_phys_0_eng_9_engtype_3D", f64::NAN),
            inst("pid_4_luid_0x0_0xA_phys_0_eng_8_engtype_3D", -4.0),
        ];
        assert_eq!(gpu_percent(&wild), Some(100.0));
        // A name without the usual structure still counts, as its own engine.
        assert_eq!(gpu_percent(&[inst("weird", 12.5)]), Some(12.5));
    }

    #[test]
    fn the_power_status_is_read_the_way_windows_reports_it() {
        // 64 % and charging on mains.
        let p = power_from_raw(1, 9, 64, 0, u32::MAX).unwrap();
        assert_eq!(
            (p.battery.percent, p.battery.charging, p.plugged),
            (64, true, true)
        );
        assert_eq!(p.secs_left, None);
        assert!(!p.saver);
        // On battery with a time estimate and the saver on.
        let p = power_from_raw(0, 1, 40, 1, 7_980).unwrap();
        assert_eq!(
            (p.battery.charging, p.plugged, p.saver),
            (false, false, true)
        );
        assert_eq!(p.secs_left, Some(7_980));
        // On battery, estimate unknown.
        assert_eq!(
            power_from_raw(0, 1, 40, 0, u32::MAX).unwrap().secs_left,
            None
        );
        // A stale estimate on mains power is not shown.
        assert_eq!(power_from_raw(1, 1, 90, 0, 1234).unwrap().secs_left, None);
        // No battery, unknown state, nonsense percentage.
        assert!(power_from_raw(1, 128, 255, 0, u32::MAX).is_none());
        assert!(power_from_raw(255, 255, 255, 0, u32::MAX).is_none());
        assert!(power_from_raw(0, 1, 101, 0, 0).is_none());
        // Full and on mains: plugged in, not charging.
        let p = power_from_raw(1, 1, 100, 0, u32::MAX).unwrap();
        assert!(p.plugged && !p.battery.charging);
    }

    #[test]
    fn history_keeps_the_newest_values_in_order() {
        let mut h = History::new(3);
        assert!(h.is_empty() && h.max() == 0.0);
        for v in [1.0, 5.0, 2.0, 3.0] {
            h.push(v);
        }
        assert_eq!(h.values(), &[5.0, 2.0, 3.0]);
        assert_eq!(h.max(), 5.0);
        assert_eq!(h.capacity(), 3);
        h.clear();
        assert!(h.is_empty());
        assert_eq!(History::new(0).capacity(), 1, "never zero-sized");
    }

    #[test]
    fn rates_read_naturally_in_bytes_and_in_bits() {
        assert_eq!(fmt_rate(0.0, false), "0 B/s");
        assert_eq!(fmt_rate(812.0, false), "812 B/s");
        assert_eq!(fmt_rate(4300.0, false), "4.2 KB/s");
        assert_eq!(fmt_rate(12.4 * 1024.0 * 1024.0, false), "12 MB/s");
        assert_eq!(fmt_rate(2.5 * 1024.0 * 1024.0 * 1024.0, false), "2.5 GB/s");
        assert_eq!(fmt_rate(-5.0, false), "0 B/s");
        assert_eq!(fmt_rate(50.0, true), "400 bps");
        assert_eq!(fmt_rate(106_250.0, true), "850 Kbps");
        assert_eq!(fmt_rate(1_225_000.0, true), "9.8 Mbps");
        assert_eq!(fmt_rate(125_000_000.0, true), "1.0 Gbps");
    }

    #[test]
    fn memory_and_remaining_time_read_naturally() {
        assert_eq!(fmt_memory(512 * 1024 * 1024), "512 MB");
        assert_eq!(fmt_memory(0), "0 MB");
        assert_eq!(fmt_memory(17_000_000_000), "15.8 GB");
        assert_eq!(fmt_remaining(30), "under a minute");
        assert_eq!(fmt_remaining(45 * 60), "45 min");
        assert_eq!(fmt_remaining(3600), "1 h");
        assert_eq!(fmt_remaining(2 * 3600 + 14 * 60 + 59), "2 h 14 min");
    }
}
