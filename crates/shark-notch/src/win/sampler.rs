//! Cursor, button and "is the user typing?" sampling for the hover state machine.
//!
//! **No hooks.** Everything here is a plain query made on a coarse timer: `GetCursorPos`,
//! `GetAsyncKeyState` for the mouse buttons, `GetLastInputInfo` and `GetGUIThreadInfo`. Typing is
//! *inferred* — "input arrived but the pointer did not move and no button was pressed" — which needs
//! no keyboard interception and never sees a keystroke's content.

use notch_core::geom::Rect;
use notch_core::hover::HoverInput;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, LASTINPUTINFO, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
    VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSORINFO, GUI_INMENUMODE, GUI_INMOVESIZE, GUI_POPUPMENUMODE, GUI_SYSTEMMENUMODE,
    GUITHREADINFO, GetCursorInfo, GetCursorPos, GetGUIThreadInfo, IDC_IBEAM, LoadCursorW,
};

use super::clock;
use super::layout::Layout;

#[derive(Default)]
pub struct Sampler {
    prev_pt: Option<(i32, i32)>,
    prev_input_tick: u32,
    typed_at: Option<f64>,
}

/// Buttons: `(currently_down, pressed_since_last_call)`.
fn buttons() -> (bool, bool) {
    let (mut down, mut pressed) = (false, false);
    for vk in [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2] {
        let s = unsafe { GetAsyncKeyState(vk.0 as i32) } as u16;
        down |= s & 0x8000 != 0;
        pressed |= s & 0x0001 != 0;
    }
    (down, pressed)
}

impl Sampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one sample. `keep_open` is the on-screen rect (hover space) that keeps an expanded notch open.
    pub fn sample(&mut self, now: f64, layout: &Layout, keep_open: Option<Rect>) -> HoverInput {
        let mut pt = POINT::default();
        let have_pt = unsafe { GetCursorPos(&mut pt) }.is_ok();
        let cursor = if have_pt {
            layout.hover_space(pt)
        } else {
            None
        };
        let moved = self.prev_pt != Some((pt.x, pt.y));
        self.prev_pt = Some((pt.x, pt.y));

        let (down, pressed) = buttons();

        // New input without pointer movement or a button press => keyboard (or wheel/touch).
        let mut li = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if unsafe { GetLastInputInfo(&mut li) }.as_bool() && li.dwTime != self.prev_input_tick {
            if !moved && !down && !pressed {
                let age = unsafe { GetTickCount() }.wrapping_sub(li.dwTime) as f64 / 1000.0;
                self.typed_at = Some(now - age.min(5.0));
            }
            self.prev_input_tick = li.dwTime;
        }

        // Only look at GUI-thread state when it can change the decision.
        let (mut move_size, mut drag) = (false, false);
        if down {
            let mut gi = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            if unsafe { GetGUIThreadInfo(0, &mut gi) }.is_ok() {
                move_size = gi.flags.0 & GUI_INMOVESIZE.0 != 0;
                let in_menu = gi.flags.0
                    & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0)
                    != 0;
                drag = !move_size && !in_menu && !gi.hwndCapture.0.is_null() && !cursor_is_ibeam();
            }
        }

        HoverInput {
            now,
            cursor,
            button_down: down,
            typed_at: self.typed_at,
            keep_open,
            drag_likely: drag,
            move_size_active: move_size,
        }
    }
}

fn cursor_is_ibeam() -> bool {
    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetCursorInfo(&mut ci) }.is_err() {
        return false;
    }
    unsafe { LoadCursorW(None, IDC_IBEAM) }
        .map(|c| c == ci.hCursor)
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn age_since(tick: u32) -> f64 {
    clock::tick_ms_to_secs(unsafe { GetTickCount() }, tick)
}
