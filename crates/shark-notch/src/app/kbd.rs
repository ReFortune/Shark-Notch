//! Keyboard input for text fields in the notch.
//!
//! The notch never takes the keyboard by itself. A module with a text field (a to-do being typed)
//! asks for it when the user clicks the field; only then does the window become focusable and
//! take the foreground. The moment the module gives the keyboard back (Enter, Escape, leaving the
//! page) or the user clicks elsewhere, the window is un-focusable again and the window that had
//! the focus before gets it back. Typing arrives as ordinary `WM_CHAR` / `WM_KEYDOWN` messages on
//! the stage window: no hook, no raw input, nothing about other applications' keystrokes.

use notch_core::input::Key;
use notch_core::module::ModuleId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_BACK, VK_CONTROL, VK_DELETE, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT,
    VK_RETURN, VK_RIGHT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindow, SetForegroundWindow};

use super::{App, Input, textclip};

/// Longest text a paste hands to a module.
const MAX_PASTE_CHARS: usize = 1000;

impl App {
    /// A module asked for the keyboard (a text field got the user's click).
    pub(crate) fn grab_keyboard(&mut self, owner: ModuleId) {
        if self.suspended() {
            return;
        }
        let Some(stage) = self.stage.as_mut() else {
            return;
        };
        if self.kbd_owner.is_none() {
            let fg = unsafe { GetForegroundWindow() };
            self.kbd_prev = (!fg.is_invalid() && fg != stage.hwnd).then_some(fg);
        }
        self.kbd_owner = Some(owner);
        stage.set_focusable(true);
        unsafe {
            let _ = SetForegroundWindow(stage.hwnd);
            let _ = SetFocus(Some(stage.hwnd));
        }
    }

    /// The module is done typing, or the user clicked elsewhere. `restore`: hand the focus back to
    /// the window that had it (not when the user has just chosen another one).
    pub(crate) fn release_keyboard(&mut self, restore: bool) {
        if self.kbd_owner.take().is_none() {
            return;
        }
        self.high_surrogate = 0;
        if let Some(stage) = self.stage.as_mut() {
            stage.set_focusable(false);
        }
        let prev = self.kbd_prev.take();
        if restore
            && let Some(prev) = prev
            && unsafe { IsWindow(Some(prev)) }.as_bool()
        {
            unsafe {
                let _ = SetForegroundWindow(prev);
            }
        }
    }

    fn keyboard_input(&mut self, input: Input) {
        let Some(owner) = self.kbd_owner else {
            return;
        };
        self.host_ctx(super::clock::now());
        self.host.keyboard_input(owner, &input);
        self.after_host();
    }

    /// `WM_CHAR`: one UTF-16 code unit of typed text.
    pub(crate) fn on_char(&mut self, unit: u32) {
        if self.kbd_owner.is_none() {
            return;
        }
        let c = match unit {
            0xD800..=0xDBFF => {
                self.high_surrogate = unit as u16;
                return;
            }
            0xDC00..=0xDFFF => {
                let hi = std::mem::take(&mut self.high_surrogate);
                if hi == 0 {
                    return;
                }
                char::decode_utf16([hi, unit as u16])
                    .next()
                    .and_then(Result::ok)
            }
            _ => char::from_u32(unit),
        };
        // Enter, Backspace, Escape and Ctrl+letter also produce control characters: those are
        // handled as keys, never as text.
        if let Some(c) = c.filter(|c| !c.is_control()) {
            self.keyboard_input(Input::Char(c));
        }
    }

    /// `WM_KEYDOWN`. Returns whether the key was handled (the message is then swallowed).
    pub(crate) fn on_keydown(&mut self, vk: u32) -> bool {
        if self.kbd_owner.is_none() {
            return false;
        }
        let key = match vk {
            v if v == u32::from(VK_RETURN.0) => Key::Enter,
            v if v == u32::from(VK_ESCAPE.0) => Key::Escape,
            v if v == u32::from(VK_BACK.0) => Key::Backspace,
            v if v == u32::from(VK_DELETE.0) => Key::Delete,
            v if v == u32::from(VK_LEFT.0) => Key::Left,
            v if v == u32::from(VK_RIGHT.0) => Key::Right,
            v if v == u32::from(VK_HOME.0) => Key::Home,
            v if v == u32::from(VK_END.0) => Key::End,
            v if v == u32::from(VK_TAB.0) => Key::Tab,
            0x56 /* 'V' */ if unsafe { GetKeyState(i32::from(VK_CONTROL.0)) } < 0 => {
                if let Some(hwnd) = self.stage.as_ref().map(|s| s.hwnd)
                    && let Some(text) = textclip::read_text(hwnd, MAX_PASTE_CHARS)
                {
                    self.keyboard_input(Input::Text(text.into()));
                }
                return true;
            }
            _ => return false,
        };
        self.keyboard_input(Input::Key(key));
        true
    }

    /// `WM_KILLFOCUS`: the user clicked another window. Finish editing; do not steal the focus back.
    pub(crate) fn on_kill_focus(&mut self) {
        if self.kbd_owner.is_some() {
            self.keyboard_input(Input::FocusLost);
            self.release_keyboard(false);
        }
    }
}
