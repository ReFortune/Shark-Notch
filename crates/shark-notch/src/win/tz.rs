//! The user's time zone, as Windows knows it (daylight-saving rules included), for the calendar:
//! the ICS reader converts between UTC and local wall-clock times through [`LocalZone`].

use notch_core::civil::{civil_from_unix, unix_from_civil};
use notch_core::ics::LocalZone;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::Time::{
    SystemTimeToTzSpecificLocalTime, TzSpecificLocalTimeToSystemTime,
};

/// The currently configured time zone.
pub struct WinZone;

fn to_systemtime(secs: i64) -> Option<SYSTEMTIME> {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    // The Win32 time-zone functions are defined for these years only.
    if !(1601..=30_827).contains(&y) {
        return None;
    }
    Some(SYSTEMTIME {
        wYear: y as u16,
        wMonth: mo as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: h as u16,
        wMinute: mi as u16,
        wSecond: s as u16,
        wMilliseconds: 0,
    })
}

fn from_systemtime(t: &SYSTEMTIME) -> i64 {
    unix_from_civil(
        i32::from(t.wYear),
        u32::from(t.wMonth),
        u32::from(t.wDay),
        u32::from(t.wHour),
        u32::from(t.wMinute),
        u32::from(t.wSecond),
    )
}

impl LocalZone for WinZone {
    fn local_to_utc(&self, wall: i64) -> i64 {
        let Some(local) = to_systemtime(wall) else {
            return wall;
        };
        let mut utc = SYSTEMTIME::default();
        match unsafe { TzSpecificLocalTimeToSystemTime(None, &local, &mut utc) } {
            Ok(()) => from_systemtime(&utc),
            Err(_) => wall,
        }
    }

    fn utc_to_local(&self, utc: i64) -> i64 {
        let Some(u) = to_systemtime(utc) else {
            return utc;
        };
        let mut local = SYSTEMTIME::default();
        match unsafe { SystemTimeToTzSpecificLocalTime(None, &u, &mut local) } {
            Ok(()) => from_systemtime(&local),
            Err(_) => utc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_and_local_agree_with_each_other_and_with_a_whole_quarter_hour_offset() {
        let z = WinZone;
        for (y, m, d) in [(2026, 1, 15), (2026, 7, 15), (2026, 10, 6), (2027, 3, 1)] {
            let noon_utc = unix_from_civil(y, m, d, 12, 0, 0);
            let local = z.utc_to_local(noon_utc);
            let off = local - noon_utc;
            assert!(
                off % 900 == 0 && off.abs() <= 14 * 3600,
                "{y}-{m}-{d}: offset {off}"
            );
            assert_eq!(z.local_to_utc(local), noon_utc, "round trip {y}-{m}-{d}");
        }
    }

    #[test]
    fn dates_outside_the_supported_range_are_passed_through() {
        let z = WinZone;
        assert_eq!(z.utc_to_local(-100_000_000_000_000), -100_000_000_000_000);
        assert_eq!(z.local_to_utc(i64::MAX / 2), i64::MAX / 2);
    }
}
