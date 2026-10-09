//! Bluetooth devices that are connected right now, with their battery level when Windows has one.
//!
//! Windows keeps both on the device's node as plain properties, so nothing here talks to a device
//! or opens a radio: `SetupDiGetDevicePropertyW` on the `BTHENUM` (classic) and `BTHLE` nodes.
//!
//! * battery: `{104EA319-6EE2-4701-BD47-8DDBF425BBE5}` 2, one byte, 0..=100 (present when the
//!   device reports its battery over a profile Windows understands);
//! * connected: `{83DA6326-97A6-4088-9453-A1923F573B29}` 15, a Boolean.
//!
//! It is read on request only (the stats page asks while it is on screen); each reading is a few
//! milliseconds on the stats worker thread.

use std::sync::Arc;

use notch_core::events::BtDevice;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_ALLCLASSES, DIGCF_PRESENT, HDEVINFO, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
    SetupDiGetDevicePropertyW,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_FriendlyName, DEVPKEY_NAME, DEVPROPTYPE};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::core::{GUID, PCWSTR};

const BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104EA319_6EE2_4701_BD47_8DDBF425BBE5),
    pid: 2,
};
const CONNECTED: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29),
    pid: 15,
};

/// Frees the device list however the function returns.
struct DevInfo(HDEVINFO);

impl Drop for DevInfo {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

fn property(set: HDEVINFO, info: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 512];
    let mut ty = DEVPROPTYPE(0);
    let mut need = 0u32;
    unsafe {
        SetupDiGetDevicePropertyW(set, info, key, &mut ty, Some(&mut buf), Some(&mut need), 0)
    }
    .ok()?;
    buf.truncate((need as usize).min(buf.len()));
    Some(buf)
}

fn string_property(set: HDEVINFO, info: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
    let bytes = property(set, info, key)?;
    let wide: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&c| c != 0)
        .collect();
    let s = String::from_utf16_lossy(&wide);
    (!s.trim().is_empty()).then_some(s)
}

fn instance_id(set: HDEVINFO, info: &SP_DEVINFO_DATA) -> String {
    let mut buf = [0u16; 200];
    if unsafe { SetupDiGetDeviceInstanceIdW(set, info, Some(&mut buf), None) }.is_err() {
        return String::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// The 12-digit hardware address a node belongs to: `DEV_<address>` for the device itself, and the
/// trailing `<address>_C…` for each of its service nodes (the headset's hands-free node, say).
fn address(id: &str) -> Option<String> {
    let hex = |s: &str| {
        s.len() >= 12 && s.is_char_boundary(12) && s[..12].chars().all(|c| c.is_ascii_hexdigit())
    };
    if let Some(i) = id.find("DEV_") {
        return hex(&id[i + 4..]).then(|| id[i + 4..i + 16].to_ascii_uppercase());
    }
    let tail = &id[id.rfind('&')? + 1..];
    hex(tail).then(|| tail[..12].to_ascii_uppercase())
}

/// The connected Bluetooth devices (classic and low energy), each once, with the battery level
/// where Windows has one. The device's own node says whether it is connected and what it is called;
/// the battery is reported on whichever of its service nodes speaks a profile that carries it (for
/// a headset, the hands-free one), so the nodes are joined by hardware address. Sorted by name.
pub fn connected() -> Vec<BtDevice> {
    // address -> (name, connected, battery)
    let mut nodes: Vec<(String, Option<String>, bool, Option<u8>)> = Vec::new();
    for enumerator in ["BTHENUM", "BTHLE"] {
        let name: Vec<u16> = enumerator.encode_utf16().chain([0]).collect();
        let Ok(raw) = (unsafe {
            SetupDiGetClassDevsW(
                None,
                PCWSTR(name.as_ptr()),
                None,
                DIGCF_ALLCLASSES | DIGCF_PRESENT,
            )
        }) else {
            continue;
        };
        let set = DevInfo(raw);
        let mut index = 0;
        loop {
            let mut info = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut info) }.is_err() {
                break;
            }
            index += 1;
            let id = instance_id(set.0, &info);
            let Some(addr) = address(&id) else { continue };
            let device = id.contains(r"\DEV_");
            let battery = property(set.0, &info, &BATTERY)
                .and_then(|b| b.first().copied())
                .filter(|&p| p <= 100);
            let (name, connected) = if device {
                (
                    string_property(set.0, &info, &DEVPKEY_Device_FriendlyName)
                        .or_else(|| string_property(set.0, &info, &DEVPKEY_NAME)),
                    property(set.0, &info, &CONNECTED).is_some_and(|b| b.first() == Some(&0xFF)),
                )
            } else {
                (None, false)
            };
            nodes.push((addr, name, connected, battery));
        }
    }
    let mut found: Vec<BtDevice> = Vec::new();
    for (addr, name, connected, _) in &nodes {
        let (true, Some(name)) = (*connected, name) else {
            continue;
        };
        let battery = nodes.iter().filter(|n| n.0 == *addr).find_map(|n| n.3);
        match found.iter_mut().find(|d| *d.name == **name) {
            Some(d) => d.battery = d.battery.or(battery),
            None => found.push(BtDevice {
                name: Arc::from(name.as_str()),
                battery,
            }),
        }
    }
    found.sort_by_key(|d| d.name.to_lowercase());
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_are_joined_by_their_hardware_address() {
        assert_eq!(
            address(r"BTHENUM\DEV_00A41CF218E6\7&3F54149&0&BLUETOOTHDEVICE_00A41CF218E6")
                .as_deref(),
            Some("00A41CF218E6")
        );
        assert_eq!(
            address(r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&0002054C_PID&0F1E\7&3F54149&0&00A41CF218E6_C00000000").as_deref(),
            Some("00A41CF218E6")
        );
        assert_eq!(
            address(r"BTHLE\DEV_5722F6C42CE3\7&28DAC1E8&0&5722F6C42CE3").as_deref(),
            Some("5722F6C42CE3")
        );
        assert_eq!(address(r"BTH\MS_BTHBRB\6&3B5B1B68&0&1"), None);
        assert_eq!(address(""), None);
    }

    #[test]
    fn listing_connected_devices_works_on_this_machine() {
        // Whatever is connected here (possibly nothing): the call must return, and what it
        // returns must be sane.
        let devices = connected();
        for d in &devices {
            assert!(!d.name.is_empty());
            assert!(d.battery.is_none_or(|b| b <= 100));
            eprintln!("bluetooth device: {} battery {:?}", d.name, d.battery);
        }
    }
}
