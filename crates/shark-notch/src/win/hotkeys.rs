//! Global hotkeys via `RegisterHotKey` only (the one global-input mechanism this app uses).

use notch_core::config::Hotkeys as HotkeyCfg;
use notch_core::hotkey;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey,
};

pub const ID_TOGGLE: i32 = 1;
pub const ID_PEEK: i32 = 2;
pub const ID_NEXT: i32 = 3;
pub const ID_PREV: i32 = 4;
const MOD_NOREPEAT: u32 = 0x4000;

#[derive(Default)]
pub struct Hotkeys {
    registered: Vec<i32>,
}

impl Hotkeys {
    /// (Re-)register everything in `cfg`. Returns human-readable problems (e.g. key already taken).
    pub fn apply(&mut self, hwnd: HWND, cfg: &HotkeyCfg) -> Vec<String> {
        self.clear(hwnd);
        let mut problems = Vec::new();
        for (id, name, spec) in [
            (ID_TOGGLE, "toggle", &cfg.toggle),
            (ID_PEEK, "peek", &cfg.peek),
            (ID_NEXT, "next_page", &cfg.next_page),
            (ID_PREV, "prev_page", &cfg.prev_page),
        ] {
            let Ok(Some(hk)) = hotkey::parse(spec) else {
                continue;
            };
            match unsafe { RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(hk.mods | MOD_NOREPEAT), hk.vk) } {
                Ok(()) => self.registered.push(id),
                Err(e) => problems.push(format!("hotkey '{spec}' ({name}) could not be registered: {e} (is another app using it?)")),
            }
        }
        problems
    }

    pub fn clear(&mut self, hwnd: HWND) {
        for id in self.registered.drain(..) {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), id);
            }
        }
    }
}
