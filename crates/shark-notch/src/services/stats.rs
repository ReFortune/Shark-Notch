//! System statistics: processor, memory, network, GPU and battery, read only when asked.
//!
//! One worker thread that sleeps in `recv()` until the stats page asks for a reading
//! (`StatsCmd::Sample`, sent by the host's poll while the page is on screen and never otherwise),
//! takes it, and publishes one `Stats` event. There is no timer in this file: when the page is not
//! open the thread is asleep and nothing is read.
//!
//! The readings (all documented, none needs elevation):
//! * processor: `GetSystemTimes` (busy share between two readings);
//! * memory: `GlobalMemoryStatusEx`;
//! * network: `GetIfTable2` byte counters summed over the *physical* adapters that are up (virtual
//!   adapters such as VPN, Hyper-V and WSL switches carry the same bytes again and are left out);
//! * GPU: the "GPU Engine" performance counters through PDH, the busiest engine (what Task
//!   Manager's headline figure is). Machines without these counters simply have no GPU figure;
//! * battery: `GetSystemPowerStatus`.
//!
//! A rate needs two readings. The first request after the page was closed therefore takes a
//! baseline, waits a quarter of a second *on this thread*, and takes the second; later requests use
//! the previous reading and answer at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notch_core::bus::BusSender;
use notch_core::events::{BtDevice, EventKind, Source, StatsSnapshot};
use notch_core::module::StatsCmd;
use notch_core::stats::{
    CpuTimes, NetCounters, cpu_percent, gpu_percent, net_rate, power_from_raw,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, FILETIME};
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::core::PCWSTR;

use crate::win::util::wide;

/// A reading older than this is not used as the baseline for a rate (the page was closed meanwhile).
const FRESH: Duration = Duration::from_secs(3);
/// How long the first reading of a session waits for its second.
const BASELINE_WAIT: Duration = Duration::from_millis(250);
/// After this long without a request the open GPU query and the last reading are released.
const RELEASE_AFTER: Duration = Duration::from_secs(5);
/// Bluetooth devices (and their batteries) are read again after this long.
const DEVICES_EVERY: Duration = Duration::from_secs(8);

enum Req {
    Sample { gpu: bool, devices: bool },
    Quit,
}

pub struct StatsService {
    tx: Sender<Req>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

impl StatsService {
    pub fn start(bus: BusSender) -> Option<StatsService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let thread = std::thread::Builder::new()
            .name("stats".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(&rx, &bus);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the stats sampler: {e}"))
            .ok()?;
        Some(StatsService {
            tx,
            thread: Some(thread),
            done,
        })
    }

    pub fn command(&self, cmd: StatsCmd) {
        let StatsCmd::Sample { gpu, devices } = cmd;
        let _ = self.tx.send(Req::Sample { gpu, devices });
    }

    pub fn stop(mut self) {
        let _ = self.tx.send(Req::Quit);
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(400);
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
    let mut sampler = Sampler::default();
    loop {
        // Asleep until asked. Once the page has been closed for a few seconds, the PDH query and
        // the previous reading are let go (the one wake-up this thread ever makes on its own), so
        // a closed page holds no memory beyond the thread itself.
        let first = if sampler.holds_resources() {
            match rx.recv_timeout(RELEASE_AFTER) {
                Ok(r) => r,
                Err(RecvTimeoutError::Timeout) => {
                    sampler.release();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(r) => r,
                Err(_) => return,
            }
        };
        let Req::Sample {
            mut gpu,
            mut devices,
        } = first
        else {
            return;
        };
        // Requests that piled up while a reading was being taken are one request.
        while let Ok(more) = rx.try_recv() {
            match more {
                Req::Quit => return,
                Req::Sample { gpu: g, devices: d } => {
                    gpu = g;
                    devices = d;
                }
            }
        }
        // The pause of a baseline reading is spent in `recv_timeout`, so quitting stays prompt.
        let mut wait = |d: Duration| match rx.recv_timeout(d) {
            Err(RecvTimeoutError::Timeout) => true,
            Ok(Req::Sample { .. }) => true,
            Ok(Req::Quit) | Err(RecvTimeoutError::Disconnected) => false,
        };
        let Some(snapshot) = sampler.sample(gpu, devices, &mut wait) else {
            return;
        };
        bus.send(Source::Local, EventKind::Stats(Arc::new(snapshot)));
    }
}

// ----- the readings --------------------------------------------------------------------------

fn ticks(f: FILETIME) -> u64 {
    (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)
}

fn cpu_times() -> Option<CpuTimes> {
    let (mut idle, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
    Some(CpuTimes {
        idle: ticks(idle),
        kernel: ticks(kernel),
        user: ticks(user),
    })
}

/// `(used, total)` physical memory in bytes.
fn memory() -> (u64, u64) {
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..MEMORYSTATUSEX::default()
    };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_err() {
        return (0, 0);
    }
    (
        m.ullTotalPhys.saturating_sub(m.ullAvailPhys),
        m.ullTotalPhys,
    )
}

/// Bytes in and out, summed over the physical adapters that are up.
fn net_counters() -> Option<NetCounters> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    if unsafe { GetIfTable2(&mut table) } != ERROR_SUCCESS || table.is_null() {
        return None;
    }
    let mut total = NetCounters::default();
    unsafe {
        let n = (*table).NumEntries as usize;
        // `Table` is declared with one element; the system allocated `NumEntries` of them.
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
        for r in rows {
            // Bit 0 of the flags: a hardware (physical) interface.
            let hardware = r.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
            if hardware && r.OperStatus == IfOperStatusUp {
                total.rx = total.rx.saturating_add(r.InOctets);
                total.tx = total.tx.saturating_add(r.OutOctets);
            }
        }
        FreeMibTable(table as *const _);
    }
    Some(total)
}

pub fn power() -> Option<notch_core::events::PowerStatus> {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.ok()?;
    power_from_raw(
        s.ACLineStatus,
        s.BatteryFlag,
        s.BatteryLifePercent,
        s.SystemStatusFlag,
        s.BatteryLifeTime,
    )
}

/// A PDH query on the GPU engine utilisation counters.
struct GpuQuery {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl GpuQuery {
    fn open() -> Option<GpuQuery> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let path = wide(r"\GPU Engine(*)\Utilization Percentage");
            let mut counter = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, PCWSTR(path.as_ptr()), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            // The first collection is only the baseline of the rate.
            let _ = PdhCollectQueryData(query);
            Some(GpuQuery { query, counter })
        }
    }

    /// `(instance name, utilisation)` for every engine instance, or `None` if the counters cannot
    /// be read right now.
    fn read(&self) -> Option<Vec<(String, f64)>> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            let rc = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                None,
            );
            if rc != PDH_MORE_DATA || size == 0 {
                return None;
            }
            // 8-byte aligned storage for the array of items and the names that follow it.
            let mut buf = vec![0u64; (size as usize).div_ceil(8) + 1];
            let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            let rc = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(items),
            );
            if rc != 0 {
                return None;
            }
            let items = std::slice::from_raw_parts(items, count as usize);
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                // 0 = valid, 1 = valid and new.
                if it.FmtValue.CStatus <= 1
                    && let Ok(name) = it.szName.to_string()
                {
                    out.push((name, it.FmtValue.Anonymous.doubleValue));
                }
            }
            Some(out)
        }
    }
}

impl Drop for GpuQuery {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

struct Reading {
    at: Instant,
    cpu: Option<CpuTimes>,
    net: Option<NetCounters>,
}

impl Reading {
    fn take() -> Reading {
        Reading {
            at: Instant::now(),
            cpu: cpu_times(),
            net: net_counters(),
        }
    }
}

#[derive(Default)]
struct Sampler {
    last: Option<Reading>,
    gpu: Option<GpuQuery>,
    /// The Bluetooth devices as last read, and when (they change slowly and cost a few ms).
    devices: Option<(Instant, Vec<BtDevice>)>,
}

impl Sampler {
    fn holds_resources(&self) -> bool {
        self.last.is_some() || self.gpu.is_some()
    }

    fn release(&mut self) {
        self.devices = None;
        self.last = None;
        self.gpu = None;
    }

    /// The connected Bluetooth devices, read again when the last reading is a few seconds old.
    fn devices(&mut self) -> Vec<BtDevice> {
        match &self.devices {
            Some((at, list)) if at.elapsed() < DEVICES_EVERY => list.clone(),
            _ => {
                let list = super::btdev::connected();
                self.devices = Some((Instant::now(), list.clone()));
                list
            }
        }
    }

    /// One snapshot. `wait` pauses for a baseline reading and returns `false` if the service is
    /// being stopped (then there is no snapshot).
    fn sample(
        &mut self,
        want_gpu: bool,
        want_devices: bool,
        wait: &mut dyn FnMut(Duration) -> bool,
    ) -> Option<StatsSnapshot> {
        let fresh = self.last.as_ref().is_some_and(|l| l.at.elapsed() < FRESH);
        if !fresh {
            // A new session on the page: take the baseline, with a fresh GPU query so that engines
            // that came and went since are not carried along.
            self.last = Some(Reading::take());
            self.gpu = None;
            if want_gpu {
                self.gpu = GpuQuery::open();
            }
            if !wait(BASELINE_WAIT) {
                return None;
            }
        }
        if !want_gpu {
            self.gpu = None;
        } else if self.gpu.is_none() {
            // Switched on while the page is open; its first reading has no baseline yet.
            self.gpu = GpuQuery::open();
        }
        let now = Reading::take();
        let prev = self.last.replace(Reading {
            at: now.at,
            cpu: now.cpu,
            net: now.net,
        });
        let secs = prev
            .as_ref()
            .map_or(0.0, |p| now.at.duration_since(p.at).as_secs_f64());
        let cpu = match (prev.as_ref().and_then(|p| p.cpu), now.cpu) {
            (Some(a), Some(b)) => cpu_percent(a, b),
            _ => None,
        };
        let net = match (prev.as_ref().and_then(|p| p.net), now.net) {
            (Some(a), Some(b)) => net_rate(a, b, secs),
            _ => None,
        };
        let gpu = self
            .gpu
            .as_ref()
            .and_then(GpuQuery::read)
            .and_then(|v| gpu_percent(&v));
        let (mem_used, mem_total) = memory();
        Some(StatsSnapshot {
            cpu,
            mem_used,
            mem_total,
            gpu,
            net,
            power: power(),
            devices: want_devices.then(|| self.devices()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notch_core::bus::{Bus, Waker};

    struct NoWake;
    impl Waker for NoWake {
        fn wake(&self) {}
    }

    #[test]
    fn the_readings_are_plausible_on_this_machine() {
        let a = cpu_times().expect("GetSystemTimes works");
        std::thread::sleep(Duration::from_millis(60));
        let b = cpu_times().unwrap();
        if let Some(p) = cpu_percent(a, b) {
            assert!((0.0..=100.0).contains(&p), "{p}");
        }
        let (used, total) = memory();
        assert!(total > 256 * 1024 * 1024, "total {total}");
        assert!(used > 0 && used <= total, "used {used} of {total}");
        // Counters exist and never go backwards between two readings (a CI VM has a virtual NIC).
        if let (Some(x), Some(y)) = (net_counters(), net_counters()) {
            assert!(y.rx >= x.rx && y.tx >= x.tx);
        }
        // Whether this machine has a battery or not, reading it must not fail badly.
        if let Some(p) = power() {
            assert!(p.battery.percent <= 100);
        }
    }

    #[test]
    fn gpu_counters_are_read_or_cleanly_absent() {
        let Some(q) = GpuQuery::open() else {
            return; // no GPU counters here (a VM without a GPU): the tile says so
        };
        std::thread::sleep(Duration::from_millis(300));
        if let Some(instances) = q.read() {
            assert!(
                instances
                    .iter()
                    .all(|(n, v)| !n.is_empty() && v.is_finite())
            );
            if let Some(g) = gpu_percent(&instances) {
                assert!((0.0..=100.0).contains(&g), "{g}");
            }
        }
    }

    #[test]
    fn a_request_is_answered_with_one_snapshot_and_nothing_is_sent_unasked() {
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = StatsService::start(tx).expect("starts");
        // Idle: nothing arrives.
        std::thread::sleep(Duration::from_millis(400));
        let mut got = Vec::new();
        bus.drain(&mut got);
        assert!(got.is_empty(), "the sampler never speaks unprompted");

        svc.command(StatsCmd::Sample {
            gpu: true,
            devices: false,
        });
        let until = Instant::now() + Duration::from_secs(10);
        let mut snap = None;
        while Instant::now() < until && snap.is_none() {
            bus.drain(&mut got);
            for ev in got.drain(..) {
                if let EventKind::Stats(s) = ev.kind {
                    snap = Some(s);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let s = snap.expect("a snapshot arrives");
        assert!(s.mem_total > 0);
        assert!(
            s.cpu.is_some(),
            "the baseline reading gives a CPU figure at once"
        );
        if let Some((d, u)) = s.net {
            assert!(d >= 0.0 && u >= 0.0);
        }

        // A second request while the page stays open answers without the baseline pause.
        let t = Instant::now();
        svc.command(StatsCmd::Sample {
            gpu: false,
            devices: false,
        });
        let mut second = None;
        while t.elapsed() < Duration::from_secs(5) && second.is_none() {
            bus.drain(&mut got);
            for ev in got.drain(..) {
                if let EventKind::Stats(s) = ev.kind {
                    second = Some(s);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = second.expect("a second snapshot");
        assert!(
            t.elapsed() < Duration::from_millis(220),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(second.gpu, None, "the GPU is read only when asked");

        let t = Instant::now();
        svc.stop();
        assert!(
            t.elapsed() < Duration::from_millis(600),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn a_sampler_lets_go_of_what_it_holds_when_released() {
        let mut sampler = Sampler::default();
        assert!(!sampler.holds_resources());
        let snap = sampler
            .sample(true, false, &mut |_| true)
            .expect("a snapshot");
        assert!(snap.mem_total > 0);
        assert!(sampler.holds_resources());
        sampler.release();
        assert!(!sampler.holds_resources() && sampler.gpu.is_none());
        // And it starts a fresh session afterwards.
        assert!(sampler.sample(false, false, &mut |_| true).is_some());
    }

    #[test]
    fn stopping_during_a_baseline_pause_is_prompt() {
        let (_bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = StatsService::start(tx).expect("starts");
        svc.command(StatsCmd::Sample {
            gpu: false,
            devices: false,
        });
        std::thread::sleep(Duration::from_millis(40)); // inside the 250 ms pause
        let t = Instant::now();
        svc.stop();
        assert!(
            t.elapsed() < Duration::from_millis(300),
            "{:?}",
            t.elapsed()
        );
    }
}
