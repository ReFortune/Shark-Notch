//! Output loudness for the media visualizer: a peak meter on the default render device.
//!
//! A tiny worker thread exists **only while the visualizer is on screen**. It samples the device's
//! peak meter ~120 times a second and keeps the maximum since the UI last asked, so a transient
//! between two frames is never missed. The UI thread only reads an atomic: a slow or hung audio
//! service can never delay a frame. No audio is captured or copied — this is the same meter the
//! Windows volume mixer draws.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notch_core::module::Audio;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};

const STARTING: u8 = 0;
const OK: u8 = 1;
const UNAVAILABLE: u8 = 2;

struct Shared {
    /// Peak (as `f32` bits; for non-negative floats the bit order equals the numeric order, so
    /// `fetch_max` is a correct float max) since the UI last read it.
    peak: AtomicU32,
    state: AtomicU8,
    stop: AtomicBool,
    done: AtomicBool,
}

pub struct AudioMeter {
    shared: Option<Arc<Shared>>,
    thread: Option<JoinHandle<()>>,
}

fn open() -> Option<IAudioMeterInformation> {
    unsafe {
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        dev.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None)
            .ok()
    }
}

fn run(s: Arc<Shared>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut meter = open();
    s.state.store(
        if meter.is_some() { OK } else { UNAVAILABLE },
        Ordering::Release,
    );
    let mut retry_at = Instant::now() + Duration::from_secs(1);
    while !s.stop.load(Ordering::Acquire) {
        match &meter {
            Some(m) => match unsafe { m.GetPeakValue() } {
                Ok(p) => {
                    s.peak
                        .fetch_max(p.clamp(0.0, 1.0).to_bits(), Ordering::AcqRel);
                }
                Err(_) => {
                    // Device removed or changed: drop it and look again shortly.
                    meter = None;
                    s.state.store(UNAVAILABLE, Ordering::Release);
                    retry_at = Instant::now() + Duration::from_millis(500);
                }
            },
            None => {
                if Instant::now() >= retry_at {
                    meter = open();
                    s.state.store(
                        if meter.is_some() { OK } else { UNAVAILABLE },
                        Ordering::Release,
                    );
                    retry_at = Instant::now() + Duration::from_secs(1);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    drop(meter);
    unsafe { CoUninitialize() };
    s.done.store(true, Ordering::Release);
}

impl AudioMeter {
    pub fn new() -> AudioMeter {
        AudioMeter {
            shared: None,
            thread: None,
        }
    }

    pub fn running(&self) -> bool {
        self.shared.is_some()
    }

    /// Start sampling (idempotent).
    pub fn start(&mut self) {
        if self.shared.is_some() {
            return;
        }
        let shared = Arc::new(Shared {
            peak: AtomicU32::new(0),
            state: AtomicU8::new(STARTING),
            stop: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        let s = shared.clone();
        match std::thread::Builder::new()
            .name("audio-meter".into())
            .stack_size(256 * 1024)
            .spawn(move || run(s))
        {
            Ok(h) => {
                self.thread = Some(h);
                self.shared = Some(shared);
            }
            Err(e) => crate::warn!("cannot start the audio meter thread: {e}"),
        }
    }

    /// Stop sampling and release the device. Never blocks the UI for long: if the audio service is
    /// hung the thread is left to finish on its own.
    pub fn stop(&mut self) {
        let Some(s) = self.shared.take() else { return };
        s.stop.store(true, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let until = Instant::now() + Duration::from_millis(60);
            while !s.done.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
            if s.done.load(Ordering::Acquire) {
                let _ = h.join();
            } // else: detached on purpose
        }
    }

    /// The loudest peak (0..=1) since the previous call, or why there is none.
    pub fn sample(&self) -> Audio {
        let Some(s) = &self.shared else {
            return Audio::Idle;
        };
        match s.state.load(Ordering::Acquire) {
            OK => Audio::Level(f32::from_bits(s.peak.swap(0, Ordering::AcqRel))),
            UNAVAILABLE => Audio::Unavailable,
            _ => Audio::Level(0.0), // still opening the device
        }
    }
}

impl Default for AudioMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioMeter {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_bits_order_like_floats_for_non_negative_values() {
        let v = [0.0f32, 0.0001, 0.2, 0.5, 0.99, 1.0];
        let by_bits: Vec<u32> = v.iter().map(|f| f.to_bits()).collect();
        let mut sorted = by_bits.clone();
        sorted.sort_unstable();
        assert_eq!(by_bits, sorted, "fetch_max on the bits is a float max");
    }

    #[test]
    fn idle_until_started_and_stoppable_without_a_device() {
        let mut m = AudioMeter::new();
        assert_eq!(m.sample(), Audio::Idle);
        m.start();
        assert!(m.running());
        // On a machine without an output device (CI) the meter reports `Unavailable`; with one it
        // reports a level. Either is fine; what matters is that it settles and stops cleanly.
        let t = Instant::now();
        let mut got = m.sample();
        while matches!(got, Audio::Level(l) if l == 0.0) && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
            got = m.sample();
        }
        assert!(matches!(got, Audio::Level(_) | Audio::Unavailable));
        m.stop();
        assert!(!m.running());
        assert_eq!(m.sample(), Audio::Idle);
    }
}
