//! Civil-calendar arithmetic on the proleptic Gregorian calendar, without time zones.
//!
//! The day-count algorithms are Howard Hinnant's public-domain "days from civil" / "civil from days"
//! formulations. Local time zones and DST are the platform's job (it hands the core a [`LocalTime`]);
//! keeping zone handling out of here keeps this module tiny and exhaustively testable.

/// Days since 1970-01-01 for a civil date.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(if m <= 2 { y - 1 } else { y });
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = u64::from((m + 9) % 12); // March = 0
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// Civil date for a day count since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// Weekday of a day count: 0 = Monday ... 6 = Sunday.
pub fn weekday_from_days(days: i64) -> u32 {
    // 1970-01-01 was a Thursday (3).
    (days + 3).rem_euclid(7) as u32
}

pub fn weekday(y: i32, m: u32, d: u32) -> u32 {
    weekday_from_days(days_from_civil(y, m, d))
}

pub fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// ISO-8601 week: `(iso_year, week 1..=53)`.
pub fn iso_week(y: i32, m: u32, d: u32) -> (i32, u32) {
    let days = days_from_civil(y, m, d);
    let thursday = days - i64::from(weekday_from_days(days)) + 3;
    let (iso_year, _, _) = civil_from_days(thursday);
    let jan1 = days_from_civil(iso_year, 1, 1);
    (iso_year, ((thursday - jan1) / 7 + 1) as u32)
}

/// Seconds since the Unix epoch for a civil date-time (interpreted as UTC).
pub fn unix_from_civil(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    days_from_civil(y, mo, d) * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(s)
}

/// Civil date-time `(y, mo, d, h, mi, s)` for a Unix timestamp (UTC).
pub fn civil_from_unix(t: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

pub const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
pub const WEEKDAYS_SHORT: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
pub const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
pub const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The wall-clock time in the user's time zone, as supplied by the platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl LocalTime {
    pub fn new(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    /// 0 = Monday ... 6 = Sunday.
    pub fn weekday(&self) -> u32 {
        weekday(self.year, self.month, self.day)
    }

    pub fn iso_week(&self) -> (i32, u32) {
        iso_week(self.year, self.month, self.day)
    }

    /// Seconds since midnight.
    pub fn seconds_of_day(&self) -> u32 {
        self.hour * 3600 + self.minute * 60 + self.second
    }

    /// Whole days since 1970-01-01 of this civil date.
    pub fn days(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }
}

impl Default for LocalTime {
    fn default() -> Self {
        LocalTime::new(1970, 1, 1, 0, 0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_day_counts() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }

    #[test]
    fn round_trips_over_eight_centuries() {
        let start = days_from_civil(1600, 1, 1);
        let end = days_from_civil(2400, 12, 31);
        let mut prev = civil_from_days(start - 1);
        for z in start..=end {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
            assert!(
                (1..=12).contains(&m) && (1..=days_in_month(y, m)).contains(&d),
                "{y}-{m}-{d}"
            );
            // Consecutive days advance by exactly one calendar day.
            let moved = (y, m, d) != prev;
            assert!(moved);
            prev = (y, m, d);
        }
    }

    #[test]
    fn weekdays_match_the_calendar() {
        assert_eq!(weekday(1970, 1, 1), 3, "Thursday");
        assert_eq!(weekday(2024, 2, 29), 3, "Thursday");
        assert_eq!(weekday(2026, 10, 6), 1, "Tuesday");
        assert_eq!(weekday(2000, 1, 1), 5, "Saturday");
        assert_eq!(weekday(1999, 12, 31), 4, "Friday");
    }

    #[test]
    fn leap_years_and_month_lengths() {
        assert!(is_leap(2000) && is_leap(2024) && !is_leap(1900) && !is_leap(2023));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2023, 12), 31);
        assert_eq!(days_in_month(2023, 4), 30);
    }

    #[test]
    fn iso_weeks_including_year_boundaries() {
        assert_eq!(iso_week(2021, 1, 3), (2020, 53));
        assert_eq!(iso_week(2020, 12, 31), (2020, 53));
        assert_eq!(iso_week(2018, 12, 31), (2019, 1));
        assert_eq!(iso_week(2026, 1, 1), (2026, 1));
        assert_eq!(iso_week(2026, 10, 6), (2026, 41));
        assert_eq!(iso_week(2024, 12, 30), (2025, 1));
    }

    #[test]
    fn unix_conversions() {
        assert_eq!(unix_from_civil(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(unix_from_civil(2001, 9, 9, 1, 46, 40), 1_000_000_000);
        assert_eq!(civil_from_unix(1_000_000_000), (2001, 9, 9, 1, 46, 40));
        assert_eq!(civil_from_unix(-1), (1969, 12, 31, 23, 59, 59));
        for t in [-86_400 * 400, -1, 0, 1, 951_782_400, 4_102_444_800] {
            let (y, mo, d, h, mi, s) = civil_from_unix(t);
            assert_eq!(unix_from_civil(y, mo, d, h, mi, s), t);
        }
    }

    #[test]
    fn local_time_helpers() {
        let t = LocalTime::new(2026, 10, 6, 14, 5, 9);
        assert_eq!(t.weekday(), 1);
        assert_eq!(t.iso_week(), (2026, 41));
        assert_eq!(t.seconds_of_day(), 14 * 3600 + 5 * 60 + 9);
        assert_eq!(WEEKDAYS[t.weekday() as usize], "Tuesday");
        assert_eq!(MONTHS_SHORT[t.month as usize - 1], "Oct");
    }
}
