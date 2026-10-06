//! The command centre's hands: master volume, panel brightness, the Wi-Fi and Bluetooth radios, and
//! a read of Windows' do-not-disturb state.
//!
//! One worker thread, **started the first time the page asks for something** and asleep in `recv()`
//! otherwise, so a command centre nobody opens costs nothing: no thread, no COM, no WinRT. It answers
//! each reading request with one `Control` event. The interfaces it opens (the audio endpoint, the
//! panel handle, the radio objects) are dropped again after half a minute without requests.
//!
//! What it uses, all documented and none needing elevation:
//! * volume and mute: `IAudioEndpointVolume` on the default render device (re-opened for every
//!   reading, so plugging in headphones is noticed);
//! * brightness: the built-in panel's `\\.\LCD` device with the `IOCTL_VIDEO_*_BRIGHTNESS` requests
//!   (the supported levels are listed by the driver; a PC without them, or with an external monitor
//!   only, simply has no brightness control here);
//! * radios: `Windows.Devices.Radios` (access is requested once; a privacy setting that refuses it
//!   shows as "not allowed");
//! * do-not-disturb: `SHQueryUserNotificationState` (read only: Windows has no supported way to
//!   change it from outside).
//!
//! Slider drags arrive as a stream of levels; those that pile up while one is being applied are
//! collapsed to the newest.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notch_core::bus::BusSender;
use notch_core::control::{combine_radios, pick_brightness, radio_from_raw, snap_brightness};
use notch_core::events::{ControlState, EventKind, Radio, Source};
use notch_core::module::{ControlCmd, RadioKind};
use windows::Devices::Radios::{
    Radio as WinRadio, RadioAccessStatus, RadioKind as WinRadioKind, RadioState,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::Shell::{QUNS_QUIET_TIME, SHQueryUserNotificationState};
use windows::core::PCWSTR;

use crate::win::util::wide;

/// Interfaces are released after this long without a request.
const IDLE: Duration = Duration::from_secs(30);

// CTL_CODE(FILE_DEVICE_VIDEO = 0x23, function, METHOD_BUFFERED, FILE_ANY_ACCESS), from ntddvdeo.h.
const IOCTL_VIDEO_QUERY_SUPPORTED_BRIGHTNESS: u32 = 0x0023_0494;
const IOCTL_VIDEO_QUERY_DISPLAY_BRIGHTNESS: u32 = 0x0023_0498;
const IOCTL_VIDEO_SET_DISPLAY_BRIGHTNESS: u32 = 0x0023_049C;
/// `DISPLAYPOLICY_BOTH`: the mains and battery values are both set.
const POLICY_BOTH: u8 = 3;

enum Req {
    Cmd(ControlCmd),
    Quit,
}

pub struct ControlService {
    tx: Sender<Req>,
    thread: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
}

impl ControlService {
    pub fn start(bus: BusSender) -> Option<ControlService> {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let thread = std::thread::Builder::new()
            .name("control".into())
            .stack_size(1 << 20)
            .spawn(move || {
                run(&rx, &bus);
                d2.store(true, Ordering::Release);
            })
            .map_err(|e| crate::warn!("cannot start the command-centre worker: {e}"))
            .ok()?;
        Some(ControlService {
            tx,
            thread: Some(thread),
            done,
        })
    }

    pub fn command(&self, cmd: ControlCmd) {
        let _ = self.tx.send(Req::Cmd(cmd));
    }

    pub fn stop(mut self) {
        let _ = self.tx.send(Req::Quit);
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

struct WinRt(bool);

impl WinRt {
    fn mta() -> WinRt {
        WinRt(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for WinRt {
    fn drop(&mut self) {
        if self.0 {
            unsafe { RoUninitialize() };
        }
    }
}

// ----- volume --------------------------------------------------------------------------------

fn endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

fn read_volume(ep: &IAudioEndpointVolume) -> Option<(f32, bool)> {
    unsafe {
        let level = ep.GetMasterVolumeLevelScalar().ok()?;
        let muted = ep.GetMute().ok()?.as_bool();
        Some((level.clamp(0.0, 1.0), muted))
    }
}

// ----- brightness ----------------------------------------------------------------------------

/// The built-in panel's brightness interface.
struct Lcd {
    handle: HANDLE,
    /// Levels the driver supports, in percent.
    levels: Vec<u8>,
}

impl Lcd {
    fn open() -> Option<Lcd> {
        let path = wide(r"\\.\LCD");
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .ok()?;
        let mut buf = [0u8; 256];
        let mut returned = 0u32;
        let listed = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_VIDEO_QUERY_SUPPORTED_BRIGHTNESS,
                None,
                0,
                Some(buf.as_mut_ptr().cast()),
                buf.len() as u32,
                Some(&mut returned),
                None,
            )
        };
        let levels: Vec<u8> = buf[..(returned as usize).min(buf.len())]
            .iter()
            .copied()
            .filter(|&l| l <= 100)
            .collect();
        if listed.is_err() || levels.is_empty() {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        Some(Lcd { handle, levels })
    }

    /// The current level as a fraction 0..=1.
    fn get(&self) -> Option<f32> {
        let mut db = [0u8; 3]; // DISPLAY_BRIGHTNESS { policy, mains, battery }
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_VIDEO_QUERY_DISPLAY_BRIGHTNESS,
                None,
                0,
                Some(db.as_mut_ptr().cast()),
                db.len() as u32,
                Some(&mut returned),
                None,
            )
        }
        .ok()?;
        let level = pick_brightness(db[0], db[1], db[2], on_mains());
        Some((f32::from(level.min(100)) / 100.0).clamp(0.0, 1.0))
    }

    fn set(&self, fraction: f32) -> bool {
        let Some(level) = snap_brightness(&self.levels, fraction) else {
            return false;
        };
        let db = [POLICY_BOTH, level, level];
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_VIDEO_SET_DISPLAY_BRIGHTNESS,
                Some(db.as_ptr().cast()),
                db.len() as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .is_ok()
    }
}

impl Drop for Lcd {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn on_mains() -> bool {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.is_ok() && s.ACLineStatus == 1
}

// ----- radios --------------------------------------------------------------------------------

fn radios() -> Vec<WinRadio> {
    let Ok(view) = WinRadio::GetRadiosAsync().and_then(|op| op.join()) else {
        return Vec::new();
    };
    let n = view.Size().unwrap_or(0);
    (0..n).filter_map(|i| view.GetAt(i).ok()).collect()
}

fn kind_matches(r: &WinRadio, kind: RadioKind) -> bool {
    matches!(
        (r.Kind(), kind),
        (Ok(WinRadioKind::WiFi), RadioKind::Wifi)
            | (Ok(WinRadioKind::Bluetooth), RadioKind::Bluetooth)
    )
}

fn do_not_disturb() -> Option<bool> {
    unsafe { SHQueryUserNotificationState() }
        .ok()
        .map(|s| s == QUNS_QUIET_TIME)
}

// ----- the worker ----------------------------------------------------------------------------

/// What is held open between requests.
#[derive(Default)]
struct Devices {
    volume: Option<IAudioEndpointVolume>,
    lcd: Option<Lcd>,
    lcd_checked: bool,
    /// The radio access answer (asked once; asked again while it was refusing).
    access: Option<RadioAccessStatus>,
}

impl Devices {
    fn volume(&mut self) -> Option<&IAudioEndpointVolume> {
        if self.volume.is_none() {
            self.volume = endpoint();
        }
        self.volume.as_ref()
    }

    fn lcd(&mut self) -> Option<&Lcd> {
        if !self.lcd_checked {
            self.lcd = Lcd::open();
            self.lcd_checked = true;
        }
        self.lcd.as_ref()
    }

    fn radio_access(&mut self) -> RadioAccessStatus {
        if self.access != Some(RadioAccessStatus::Allowed) {
            self.access = Some(
                WinRadio::RequestAccessAsync()
                    .and_then(|op| op.join())
                    .unwrap_or(RadioAccessStatus::Unspecified),
            );
        }
        self.access.unwrap_or(RadioAccessStatus::Unspecified)
    }

    /// Wi-Fi and Bluetooth as the page shows them.
    fn read_radios(&mut self) -> (Radio, Radio) {
        let access = self.radio_access();
        if access != RadioAccessStatus::Allowed && access != RadioAccessStatus::Unspecified {
            return (Radio::Denied, Radio::Denied);
        }
        let all = radios();
        let of = |kind: RadioKind| -> Radio {
            let states: Vec<Radio> = all
                .iter()
                .filter(|r| kind_matches(r, kind))
                .map(|r| r.State().map_or(Radio::Disabled, |s| radio_from_raw(s.0)))
                .collect();
            combine_radios(&states)
        };
        (of(RadioKind::Wifi), of(RadioKind::Bluetooth))
    }

    fn read(&mut self) -> ControlState {
        // The default output device can change at any time: look it up again for every reading.
        self.volume = None;
        let volume = self.volume().and_then(read_volume);
        let brightness = self.lcd().and_then(Lcd::get);
        let (wifi, bluetooth) = self.read_radios();
        ControlState {
            volume,
            brightness,
            wifi,
            bluetooth,
            dnd: do_not_disturb(),
        }
    }

    fn set_radio(&mut self, kind: RadioKind, on: bool) {
        if self.radio_access() != RadioAccessStatus::Allowed {
            return;
        }
        let state = if on { RadioState::On } else { RadioState::Off };
        for r in radios().iter().filter(|r| kind_matches(r, kind)) {
            if let Err(e) = r.SetStateAsync(state).and_then(|op| op.join()) {
                crate::warn!("could not switch a radio ({e})");
            }
        }
    }
}

fn run(rx: &Receiver<Req>, bus: &BusSender) {
    let _rt = WinRt::mta();
    let mut dev = Devices::default();
    let mut warm = false;
    loop {
        // Asleep until asked; after half a minute of nothing, let go of what was opened.
        let first = if warm {
            match rx.recv_timeout(IDLE) {
                Ok(r) => r,
                Err(RecvTimeoutError::Timeout) => {
                    dev = Devices::default();
                    warm = false;
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
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        let mut cmds = Vec::with_capacity(batch.len());
        for r in batch {
            match r {
                Req::Quit => return,
                Req::Cmd(c) => cmds.push(c),
            }
        }
        warm = true;
        let mut answer = false;
        for (i, c) in cmds.iter().enumerate() {
            // A level that a later one in the same batch replaces is not worth applying.
            let superseded = |same: fn(&ControlCmd) -> bool| cmds[i + 1..].iter().any(same);
            match *c {
                ControlCmd::Refresh => answer = true,
                ControlCmd::SetVolume(v) => {
                    if !superseded(|c| matches!(c, ControlCmd::SetVolume(_)))
                        && let Some(ep) = dev.volume().cloned()
                    {
                        unsafe {
                            let _ =
                                ep.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null());
                        }
                    }
                }
                ControlCmd::ToggleMute => {
                    if let Some(ep) = dev.volume().cloned() {
                        unsafe {
                            if let Ok(m) = ep.GetMute() {
                                let _ = ep.SetMute(!m.as_bool(), std::ptr::null());
                            }
                        }
                    }
                    answer = true;
                }
                ControlCmd::SetBrightness(b) => {
                    if !superseded(|c| matches!(c, ControlCmd::SetBrightness(_)))
                        && let Some(lcd) = dev.lcd()
                    {
                        let _ = lcd.set(b);
                    }
                }
                ControlCmd::SetRadio { kind, on } => {
                    dev.set_radio(kind, on);
                    answer = true;
                }
                // These open Windows pages; the UI thread does that (see `App::exec`).
                ControlCmd::Snip
                | ControlCmd::OpenFocusSettings
                | ControlCmd::OpenRadioSettings => {}
            }
        }
        if answer {
            bus.send(Source::Local, EventKind::Control(Arc::new(dev.read())));
        }
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
    fn a_reading_never_fails_badly_on_a_machine_without_the_hardware() {
        let _rt = WinRt::mta();
        let mut dev = Devices::default();
        let s = dev.read();
        // A CI machine has no audio device, panel or radios; a laptop has some. Either way the
        // answer is well-formed and the levels are in range.
        if let Some((v, _)) = s.volume {
            assert!((0.0..=1.0).contains(&v));
        }
        if let Some(b) = s.brightness {
            assert!((0.0..=1.0).contains(&b));
        }
    }

    #[test]
    fn a_request_is_answered_once_and_nothing_is_sent_unasked() {
        let (mut bus, tx) = Bus::new(Arc::new(NoWake));
        let svc = ControlService::start(tx).expect("starts");
        std::thread::sleep(Duration::from_millis(300));
        let mut got = Vec::new();
        bus.drain(&mut got);
        assert!(got.is_empty(), "silent until asked");

        svc.command(ControlCmd::Refresh);
        let until = Instant::now() + Duration::from_secs(15);
        let mut answer = None;
        while Instant::now() < until && answer.is_none() {
            bus.drain(&mut got);
            for ev in got.drain(..) {
                if let EventKind::Control(s) = ev.kind {
                    answer = Some(s);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(answer.is_some(), "a reading arrives");

        // (Level, mute and radio commands are deliberately not exercised here: on a real
        // machine they would change the volume, the brightness or the Wi-Fi of whoever runs the tests.)
        let t = Instant::now();
        svc.stop();
        assert!(
            t.elapsed() < Duration::from_millis(700),
            "{:?}",
            t.elapsed()
        );
    }
}
