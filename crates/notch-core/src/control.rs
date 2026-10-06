//! The arithmetic behind the command centre's controls, kept apart from the Windows calls so it can
//! be tested anywhere: brightness levels, which of the two stored brightness values applies, and how
//! the states of several radios of one kind read as one.

use crate::events::Radio;

/// The supported brightness level (a percentage, as drivers list them) nearest to `fraction`
/// (0..=1). A tie goes to the lower level. `None` if the driver listed no levels.
pub fn snap_brightness(levels: &[u8], fraction: f32) -> Option<u8> {
    let want = (fraction.clamp(0.0, 1.0) * 100.0).round();
    levels
        .iter()
        .copied()
        .filter(|&l| l <= 100)
        .min_by(|&a, &b| {
            let (da, db) = ((f32::from(a) - want).abs(), (f32::from(b) - want).abs());
            da.total_cmp(&db).then(a.cmp(&b))
        })
}

/// Which stored brightness applies now. The driver reports a mains value and a battery value and a
/// policy saying which are valid (1 = mains, 2 = battery, anything else = both).
pub fn pick_brightness(policy: u8, mains: u8, battery: u8, on_mains: bool) -> u8 {
    match policy {
        1 => mains,
        2 => battery,
        _ if on_mains => mains,
        _ => battery,
    }
}

/// A radio's state from Windows' `RadioState` (0 unknown, 1 on, 2 off, 3 disabled).
pub fn radio_from_raw(state: i32) -> Radio {
    match state {
        1 => Radio::On,
        2 => Radio::Off,
        _ => Radio::Disabled,
    }
}

/// Several radios of one kind (a laptop can list two Wi-Fi adapters) read as one: on if any is on.
pub fn combine_radios(states: &[Radio]) -> Radio {
    for wanted in [Radio::On, Radio::Off, Radio::Disabled, Radio::Denied] {
        if states.contains(&wanted) {
            return wanted;
        }
    }
    Radio::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_snaps_to_the_nearest_supported_level() {
        let levels = [0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(snap_brightness(&levels, 0.0), Some(0));
        assert_eq!(snap_brightness(&levels, 0.47), Some(50));
        assert_eq!(snap_brightness(&levels, 0.449), Some(40));
        assert_eq!(snap_brightness(&levels, 1.0), Some(100));
        assert_eq!(
            snap_brightness(&levels, 7.0),
            Some(100),
            "out of range clamps"
        );
        assert_eq!(snap_brightness(&levels, -3.0), Some(0));
        // A tie goes to the lower level; unsorted and nonsense entries are tolerated.
        assert_eq!(snap_brightness(&[0, 20], 0.10), Some(0));
        assert_eq!(snap_brightness(&[100, 30, 250, 0], 0.5), Some(30));
        // A driver with few steps.
        assert_eq!(snap_brightness(&[10, 100], 0.4), Some(10));
        assert_eq!(snap_brightness(&[], 0.5), None);
        assert_eq!(snap_brightness(&[200, 255], 0.5), None, "no usable level");
    }

    #[test]
    fn the_applicable_brightness_follows_the_policy_and_the_power_source() {
        assert_eq!(pick_brightness(1, 80, 40, false), 80, "mains-only policy");
        assert_eq!(pick_brightness(2, 80, 40, true), 40, "battery-only policy");
        assert_eq!(pick_brightness(3, 80, 40, true), 80);
        assert_eq!(pick_brightness(3, 80, 40, false), 40);
        assert_eq!(
            pick_brightness(0, 80, 40, false),
            40,
            "unknown policy: by power source"
        );
    }

    #[test]
    fn radio_states_read_naturally() {
        assert_eq!(radio_from_raw(1), Radio::On);
        assert_eq!(radio_from_raw(2), Radio::Off);
        assert_eq!(radio_from_raw(3), Radio::Disabled);
        assert_eq!(radio_from_raw(0), Radio::Disabled);
        assert_eq!(radio_from_raw(42), Radio::Disabled);
    }

    #[test]
    fn several_radios_read_as_one() {
        assert_eq!(combine_radios(&[]), Radio::Unavailable);
        assert_eq!(combine_radios(&[Radio::Off, Radio::On]), Radio::On);
        assert_eq!(combine_radios(&[Radio::Disabled, Radio::Off]), Radio::Off);
        assert_eq!(combine_radios(&[Radio::Disabled]), Radio::Disabled);
        assert_eq!(
            combine_radios(&[Radio::Denied, Radio::Denied]),
            Radio::Denied
        );
    }
}
