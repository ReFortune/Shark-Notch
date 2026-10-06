//! Which programs are using the microphone or the camera, read from Windows' own usage records.
//!
//! Windows' consent store (`HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager
//! \ConsentStore\{microphone,webcam}\<program>`) keeps `LastUsedTimeStart` / `LastUsedTimeStop` for
//! every program; the stop value is 0 while the device is in use. This service waits on a registry
//! change notification (`RegNotifyChangeKeyValue`) and re-reads the records when something changes:
//! no polling, no hooks, no driver, and nothing about *what* is recorded. The program names stay in
//! memory; they are not logged.
//!
//! Each class key (`microphone`, `webcam`) is watched; if Windows has not created one yet, the
//! deepest key that exists on the way down is watched instead, and the watch moves down once it
//! appears. The service sleeps in a single wait; it has no timer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notch_core::bus::BusSender;
use notch_core::events::{EventKind, PrivacyState, Source};
use notch_core::privacy::{friendly_app_name, in_use, tidy};
use windows::Win32::Foundation::{ERROR_SUCCESS, WAIT_OBJECT_0, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET,
    REG_NOTIFY_CHANGE_NAME, REG_QWORD, REG_VALUE_TYPE, RegCloseKey, RegEnumKeyExW,
    RegNotifyChangeKeyValue, RegOpenKeyExW, RegQueryValueExW,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::{PCWSTR, PWSTR};

use crate::win::util::{EventHandle, wide};

/// Path steps from `HKCU` down to the consent store.
const STORE: [&str; 6] = [
    "Software",
    "Microsoft",
    "Windows",
    "CurrentVersion",
    "CapabilityAccessManager",
    "ConsentStore",
];
const MIC: &str = "microphone";
const CAM: &str = "webcam";
/// Registry writes come in bursts (start time, then more): wait for the dust to settle.
const SETTLE: Duration = Duration::from_millis(120);

pub struct PrivacyService {
    quit: Arc<EventHandle>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

struct Key(HKEY);

impl Key {
    fn open(parent: HKEY, path: &str, notify: bool) -> Option<Key> {
        let w = wide(path);
        let mut h = HKEY::default();
        let access = if notify {
            KEY_READ | KEY_NOTIFY
        } else {
            KEY_READ
        };
        let rc = unsafe { RegOpenKeyExW(parent, PCWSTR(w.as_ptr()), None, access, &mut h) };
        (rc == ERROR_SUCCESS).then_some(Key(h))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn ok(rc: WIN32_ERROR) -> bool {
    rc == ERROR_SUCCESS
}

/// Current time as `FILETIME` ticks.
fn now_ticks() -> u64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() + 11_644_473_600) * 10_000_000 + u64::from(d.subsec_nanos() / 100)
}

/// When the machine started, as `FILETIME` ticks (from the uptime counter).
fn boot_ticks(now: u64) -> u64 {
    let up_ms = unsafe { GetTickCount64() };
    now.saturating_sub(up_ms.saturating_mul(10_000))
}

/// A `REG_QWORD` value, or 0 if it is missing.
fn qword(key: &Key, name: &str) -> u64 {
    let n = wide(name);
    let mut ty = REG_VALUE_TYPE::default();
    let mut buf = [0u8; 8];
    let mut len = 8u32;
    let rc = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(n.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        )
    };
    if ok(rc) && ty == REG_QWORD && len == 8 {
        u64::from_le_bytes(buf)
    } else {
        0
    }
}

/// Names of the sub-keys of `key`.
fn subkeys(key: &Key) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..4096u32 {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(
                key.0,
                i,
                Some(PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if !ok(rc) {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    out
}

/// The programs currently using one device class (`microphone` / `webcam`).
fn programs_using(root: &Key, class: &str, now: u64, boot: u64) -> Vec<String> {
    let Some(dev) = Key::open(root.0, class, false) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut check = |parent: &Key, sub: &str| {
        if let Some(k) = Key::open(parent.0, sub, false)
            && in_use(
                qword(&k, "LastUsedTimeStart"),
                qword(&k, "LastUsedTimeStop"),
                now,
                boot,
            )
        {
            names.push(friendly_app_name(sub));
        }
    };
    for sub in subkeys(&dev) {
        if sub.eq_ignore_ascii_case("NonPackaged") {
            if let Some(np) = Key::open(dev.0, &sub, false) {
                for app in subkeys(&np) {
                    check(&np, &app);
                }
            }
        } else {
            check(&dev, &sub);
        }
    }
    tidy(names)
}

/// Read the whole picture.
fn scan() -> PrivacyState {
    let path = STORE.join("\\");
    let Some(root) = Key::open(HKEY_CURRENT_USER, &path, false) else {
        return PrivacyState::default();
    };
    let now = now_ticks();
    let boot = boot_ticks(now);
    let to_arcs = |v: Vec<String>| v.into_iter().map(|s| Arc::from(s.as_str())).collect();
    PrivacyState {
        mic: to_arcs(programs_using(&root, MIC, now, boot)),
        camera: to_arcs(programs_using(&root, CAM, now, boot)),
    }
}

/// The deepest key on the way to `HKCU\<STORE>\<class>` that exists, opened for change
/// notifications. (Before Windows creates the class key the consent store itself is watched, and the
/// watch moves down once the class key appears.)
fn deepest_existing(class: &str) -> Key {
    let mut steps: Vec<&str> = STORE.to_vec();
    steps.push(class);
    for depth in (1..=steps.len()).rev() {
        if let Some(k) = Key::open(HKEY_CURRENT_USER, &steps[..depth].join("\\"), true) {
            return k;
        }
    }
    // `HKCU\Software` always exists; this is only reached if even that cannot be opened.
    Key::open(HKEY_CURRENT_USER, "Software", true).unwrap_or(Key(HKEY_CURRENT_USER))
}

impl PrivacyService {
    pub fn start(bus: BusSender) -> Option<PrivacyService> {
        let quit = Arc::new(EventHandle::new(true)?);
        let done = Arc::new(AtomicBool::new(false));
        let (q2, d2) = (quit.clone(), done.clone());
        let thread = std::thread::Builder::new()
            .name("privacy".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                run(bus, &q2);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the privacy watcher: {e}"))
            .ok()?;
        Some(PrivacyService {
            quit,
            thread: Some(thread),
            done,
        })
    }

    pub fn stop(mut self) {
        unsafe {
            let _ = SetEvent(self.quit.0);
        }
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(500);
            while !self.done.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.done.load(Ordering::Acquire) {
                let _ = h.join();
            }
        }
    }
}

fn run(bus: BusSender, quit: &EventHandle) {
    let Some(event) = EventHandle::new(false) else {
        crate::warn!("privacy: cannot create an event");
        return;
    };
    let (event, quit) = (event.0, quit.0);
    // `None` until the first scan, which is always published: after a suspension the module may be
    // holding a picture that is out of date.
    let mut last: Option<PrivacyState> = None;
    loop {
        // Arm the watches *before* scanning, so a change in between is never lost. Each watch fires
        // the one event; the keys are closed (and the watches with them) at the end of the iteration.
        let keys = [deepest_existing(MIC), deepest_existing(CAM)];
        let armed = keys.iter().all(|k| {
            let rc = unsafe {
                RegNotifyChangeKeyValue(
                    k.0,
                    true,
                    REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                    Some(event),
                    true,
                )
            };
            if !ok(rc) {
                crate::warn!("privacy: cannot watch the registry (error {})", rc.0);
            }
            ok(rc)
        });
        if !armed {
            break;
        }
        let state = scan();
        if last.as_ref() != Some(&state) {
            crate::debug!(
                "privacy: {} program(s) on the microphone, {} on the camera",
                state.mic.len(),
                state.camera.len()
            );
            last = Some(state.clone());
            bus.send(Source::Local, EventKind::Privacy(Arc::new(state)));
        }
        let woke = unsafe { WaitForMultipleObjects(&[event, quit], false, INFINITE) };
        if woke.0 != WAIT_OBJECT_0.0 {
            break; // the quit event (or a failed wait)
        }
        // Writes come in bursts (start time, then more): let them finish before looking.
        std::thread::sleep(SETTLE);
    }
}

/// A fake program that "uses the microphone", written into the real consent store, for the
/// self-test (`--registry-probe`) and the unit test. It is removed again straight afterwards; the
/// key is named so it cannot be mistaken for a real program.
pub mod probe {
    use super::*;
    use windows::Win32::System::Registry::{
        KEY_WRITE, REG_OPTION_NON_VOLATILE, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
    };

    /// The key under `microphone\NonPackaged` (a path with `\` written as `#`).
    pub const FAKE_KEY: &str = "C:#SharkNotchProbe#selftest-fake.exe";
    /// What [`friendly_app_name`] makes of it.
    pub const FAKE_NAME: &str = "Selftest-fake";

    fn class_path() -> String {
        format!("{}\\{MIC}\\NonPackaged", STORE.join("\\"))
    }

    /// Now, as `FILETIME` ticks.
    pub fn now() -> u64 {
        now_ticks()
    }

    /// Write the fake program's usage record (`stop == 0` means "in use right now").
    pub fn set_usage(start: u64, stop: u64) -> Result<(), String> {
        let path = wide(&format!("{}\\{FAKE_KEY}", class_path()));
        let mut key = HKEY::default();
        unsafe {
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut key,
                None,
            );
            if !ok(rc) {
                return Err(format!("cannot create the probe key (error {})", rc.0));
            }
            let mut result = Ok(());
            for (name, v) in [("LastUsedTimeStart", start), ("LastUsedTimeStop", stop)] {
                let n = wide(name);
                let rc = RegSetValueExW(
                    key,
                    PCWSTR(n.as_ptr()),
                    None,
                    REG_QWORD,
                    Some(&v.to_le_bytes()),
                );
                if !ok(rc) {
                    result = Err(format!("cannot write {name} (error {})", rc.0));
                }
            }
            let _ = RegCloseKey(key);
            result
        }
    }

    /// Remove the fake program's key (and nothing else).
    pub fn clear() {
        let parent = wide(&class_path());
        let sub = wide(FAKE_KEY);
        unsafe {
            let mut k = HKEY::default();
            if ok(RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(parent.as_ptr()),
                None,
                KEY_WRITE,
                &mut k,
            )) {
                let _ = RegDeleteTreeW(k, PCWSTR(sub.as_ptr()));
                let _ = RegCloseKey(k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::probe::{FAKE_NAME, clear, now, set_usage};
    use super::*;
    use notch_core::bus::{Bus, Waker};

    struct NoWake;
    impl Waker for NoWake {
        fn wake(&self) {}
    }

    /// The tests share one registry key: they must not run at the same time.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn wait_for(bus: &mut Bus, pred: impl Fn(&PrivacyState) -> bool) -> Option<PrivacyState> {
        let until = Instant::now() + Duration::from_secs(8);
        let mut got = Vec::new();
        while Instant::now() < until {
            bus.drain(&mut got);
            for ev in got.drain(..) {
                if let EventKind::Privacy(p) = ev.kind
                    && pred(&p)
                {
                    return Some((*p).clone());
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn a_program_starting_and_stopping_on_the_microphone_is_seen_through_the_registry() {
        let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = PrivacyService::start(tx).expect("the watcher starts");
        // The first event always describes the starting state.
        assert!(
            wait_for(&mut bus, |_| true).is_some(),
            "an initial state is published"
        );

        let start = now();
        set_usage(start, 0).expect("the probe key can be written");
        let seen = wait_for(&mut bus, |p| p.mic.iter().any(|a| &**a == FAKE_NAME));
        assert!(seen.is_some(), "the program in use shows up");

        set_usage(start, start + 50_000_000).expect("the probe key can be updated");
        let gone = wait_for(&mut bus, |p| !p.mic.iter().any(|a| &**a == FAKE_NAME));
        assert!(gone.is_some(), "and goes away when it stops");

        svc.stop();
        clear();
    }

    #[test]
    fn a_restarted_watcher_always_reports_the_current_state() {
        let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let first = PrivacyService::start(tx.clone()).expect("starts");
        assert!(wait_for(&mut bus, |_| true).is_some());
        first.stop();
        // Something changed while it was stopped (a game was in front): the next start says so.
        set_usage(now(), 0).expect("write");
        let second = PrivacyService::start(tx).expect("starts again");
        assert!(
            wait_for(&mut bus, |p| p.mic.iter().any(|a| &**a == FAKE_NAME)).is_some(),
            "the new watcher publishes what it finds, even if it equals nothing it said before"
        );
        second.stop();
        clear();
    }
}
