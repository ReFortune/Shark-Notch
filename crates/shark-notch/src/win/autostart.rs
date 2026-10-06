//! "Start with Windows" via the per-user Run key (no admin rights, trivially reversible).

use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::PCWSTR;

use super::util::{pcwstr, wide};

const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "SharkNotch";

pub fn is_enabled() -> bool {
    let (k, n) = (wide(KEY), wide(NAME));
    let mut size = 0u32;
    let rc: WIN32_ERROR = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            pcwstr(&k),
            pcwstr(&n),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    rc.0 == 0
}

pub fn set(enabled: bool) -> Result<(), String> {
    let (k, n) = (wide(KEY), wide(NAME));
    unsafe {
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let value = wide(&format!("\"{}\" --autostart", exe.display()));
            let rc = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                pcwstr(&k),
                pcwstr(&n),
                REG_SZ.0,
                Some(value.as_ptr() as *const _),
                (value.len() * 2) as u32,
            );
            if rc.0 != 0 {
                Err(format!("RegSetKeyValueW failed: {}", rc.0))
            } else {
                Ok(())
            }
        } else {
            let rc = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr()), PCWSTR(n.as_ptr()));
            // ERROR_FILE_NOT_FOUND (2) just means it was not set.
            if rc.0 != 0 && rc.0 != 2 {
                Err(format!("RegDeleteKeyValueW failed: {}", rc.0))
            } else {
                Ok(())
            }
        }
    }
}
