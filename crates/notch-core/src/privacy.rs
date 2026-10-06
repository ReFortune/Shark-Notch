//! Which programs are using the microphone or the camera, from Windows' own usage records.
//!
//! Windows keeps, per program and per device class, when it last started and stopped using the
//! device (`LastUsedTimeStart` / `LastUsedTimeStop`, Windows `FILETIME` ticks; the stop value is 0
//! while the device is in use). The platform reads those records; the rules for interpreting them
//! and naming the program live here so they can be tested without a registry.

/// 100-nanosecond ticks per second (`FILETIME`).
const TICKS_PER_SEC: u64 = 10_000_000;
/// A record that says "in use" for longer than this is a program that died while recording: the
/// stop time was never written. Ignored rather than shown forever.
const STALE_AFTER: u64 = 24 * 3600 * TICKS_PER_SEC;
/// How far a record may predate the computed boot time and still count (clock adjustments move the
/// computed boot time by a few seconds).
const BOOT_SLACK: u64 = 60 * TICKS_PER_SEC;

/// Is a device in use according to these two `FILETIME` values? `now` is the current time and
/// `boot` the time the machine started, both as ticks: a record that begins before the machine
/// started is left over from a session that ended without writing its stop time (a power cut), so
/// it cannot be a program that is recording now.
pub fn in_use(start: u64, stop: u64, now: u64, boot: u64) -> bool {
    start != 0
        && (stop == 0 || stop < start)
        && now.saturating_sub(start) < STALE_AFTER
        && start.saturating_add(BOOT_SLACK) >= boot
}

/// `Windows Camera` from `Microsoft.WindowsCamera_8wekyb3d8bbwe`, `Zoom` from
/// `C:#Program Files#Zoom#bin#Zoom.exe`. Never empty, at most 40 characters.
pub fn friendly_app_name(key: &str) -> String {
    let key = key.trim();
    let name = if key.contains('#') {
        // A desktop program: the path with `\` written as `#`; the file name is the last piece.
        let file = key.rsplit('#').next().unwrap_or(key);
        let stem = match file.rfind('.') {
            Some(i) if file[i + 1..].eq_ignore_ascii_case("exe") => &file[..i],
            _ => file,
        };
        let mut chars = stem.chars();
        match chars.next() {
            Some(c) if c.is_lowercase() => c.to_uppercase().chain(chars).collect(),
            _ => stem.to_string(),
        }
    } else {
        // A packaged app: `Publisher.Name_publisherhash`.
        let id = key.split('_').next().unwrap_or(key);
        let tail = id.rsplit('.').next().unwrap_or(id);
        // WindowsCamera -> Windows Camera
        let mut out = String::new();
        let mut prev_lower = false;
        for c in tail.chars() {
            if c.is_uppercase() && prev_lower {
                out.push(' ');
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
            out.push(c);
        }
        out
    };
    let name: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
    if name.trim().is_empty() {
        "an app".to_string()
    } else {
        name.trim().to_string()
    }
}

/// Merge names into the sorted, de-duplicated list the module shows.
pub fn tidy(mut names: Vec<String>) -> Vec<String> {
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 133_000_000_000_000_000;
    /// The machine started a week ago.
    const BOOT: u64 = NOW - 7 * 24 * 3600 * TICKS_PER_SEC;

    #[test]
    fn a_zero_stop_time_means_in_use_and_a_later_stop_means_done() {
        let start = NOW - 60 * TICKS_PER_SEC;
        assert!(
            in_use(start, 0, NOW, BOOT),
            "stop is 0 while the device is in use"
        );
        assert!(
            !in_use(start, start + 30 * TICKS_PER_SEC, NOW, BOOT),
            "stopped after it started"
        );
        assert!(
            in_use(start, start - 5, NOW, BOOT),
            "a stop older than the start belongs to the previous use"
        );
        assert!(!in_use(0, 0, NOW, BOOT), "never used");
    }

    #[test]
    fn a_record_stuck_on_in_use_for_a_day_is_ignored() {
        let long_ago = NOW - 25 * 3600 * TICKS_PER_SEC;
        assert!(!in_use(long_ago, 0, NOW, BOOT));
        let recent = NOW - 23 * 3600 * TICKS_PER_SEC;
        assert!(in_use(recent, 0, NOW, BOOT), "a long call is still a call");
        assert!(
            in_use(NOW + 5, 0, NOW, BOOT),
            "a start slightly in the future (clock skew) still counts"
        );
    }

    #[test]
    fn a_record_from_before_the_machine_started_cannot_be_in_use() {
        let boot = NOW - 2 * 3600 * TICKS_PER_SEC;
        let before_boot = boot - 600 * TICKS_PER_SEC;
        assert!(
            !in_use(before_boot, 0, NOW, boot),
            "left over from before a power cut"
        );
        let just_after = boot + 30 * TICKS_PER_SEC;
        assert!(in_use(just_after, 0, NOW, boot), "an early-boot call");
        let within_slack = boot - 30 * TICKS_PER_SEC;
        assert!(
            in_use(within_slack, 0, NOW, boot),
            "a computed boot time can be a few seconds off"
        );
    }

    #[test]
    fn desktop_programs_are_named_after_their_file() {
        assert_eq!(
            friendly_app_name("C:#Program Files#Zoom#bin#Zoom.exe"),
            "Zoom"
        );
        assert_eq!(
            friendly_app_name("C:#Users#me#AppData#Local#Discord#app-1.0#Discord.exe"),
            "Discord"
        );
        assert_eq!(
            friendly_app_name("C:#Program Files#Google#Chrome#Application#chrome.exe"),
            "Chrome"
        );
        assert_eq!(friendly_app_name("C:#tools#obs64.EXE"), "Obs64");
        assert_eq!(friendly_app_name("C:#x#tool"), "Tool", "no extension");
        assert_eq!(
            friendly_app_name("C:#x#"),
            "an app",
            "nothing after the last #"
        );
    }

    #[test]
    fn packaged_apps_are_named_from_their_package() {
        assert_eq!(
            friendly_app_name("Microsoft.WindowsCamera_8wekyb3d8bbwe"),
            "Windows Camera"
        );
        assert_eq!(friendly_app_name("MSTeams_8wekyb3d8bbwe"), "MSTeams");
        assert_eq!(
            friendly_app_name("Microsoft.SkypeApp_kzf8qxf38zg5c"),
            "Skype App"
        );
        assert_eq!(
            friendly_app_name("SpotifyAB.SpotifyMusic_zpdnekdrzrea0"),
            "Spotify Music"
        );
        assert_eq!(friendly_app_name(""), "an app");
    }

    #[test]
    fn names_are_bounded_and_clean() {
        let long = format!("C:#x#{}.exe", "a".repeat(200));
        assert!(friendly_app_name(&long).chars().count() <= 40);
        assert_eq!(friendly_app_name("C:#x#bad\u{7}name.exe"), "Badname");
    }

    #[test]
    fn the_list_is_sorted_and_unique_regardless_of_case() {
        assert_eq!(
            tidy(vec![
                "Zoom".into(),
                "chrome".into(),
                "Chrome".into(),
                "Alpha".into()
            ]),
            vec!["Alpha", "chrome", "Zoom"]
        );
        assert!(tidy(vec![]).is_empty());
    }
}
