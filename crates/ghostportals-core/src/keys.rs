//! Physical key codes. Keys travel as Linux evdev codes; Windows reads and
//! writes PC set-1 scan codes, which match evdev for every key that has no
//! `E0` prefix. The rest are listed here.

/// `(scan code, extended, evdev)` for keys whose evdev code differs from the
/// scan code.
const EXTENDED: &[(u16, bool, u32)] = &[
    (0x1c, true, 96),   // KEY_KPENTER
    (0x1d, true, 97),   // KEY_RIGHTCTRL
    (0x35, true, 98),   // KEY_KPSLASH
    (0x37, true, 99),   // KEY_SYSRQ (Print Screen)
    (0x38, true, 100),  // KEY_RIGHTALT
    (0x45, false, 119), // KEY_PAUSE (Windows reports it without E0)
    (0x45, true, 69),   // KEY_NUMLOCK (and with it)
    (0x47, true, 102),  // KEY_HOME
    (0x48, true, 103),  // KEY_UP
    (0x49, true, 104),  // KEY_PAGEUP
    (0x4b, true, 105),  // KEY_LEFT
    (0x4d, true, 106),  // KEY_RIGHT
    (0x4f, true, 107),  // KEY_END
    (0x50, true, 108),  // KEY_DOWN
    (0x51, true, 109),  // KEY_PAGEDOWN
    (0x52, true, 110),  // KEY_INSERT
    (0x53, true, 111),  // KEY_DELETE
    (0x5b, true, 125),  // KEY_LEFTMETA
    (0x5c, true, 126),  // KEY_RIGHTMETA
    (0x5d, true, 127),  // KEY_COMPOSE (Menu)
];

/// Evdev code for a Windows scan code, if it has one.
pub fn evdev_from_scancode(scancode: u16, extended: bool) -> Option<u32> {
    if let Some(&(_, _, evdev)) = EXTENDED.iter().find(|(s, e, _)| *s == scancode && *e == extended)
    {
        return Some(evdev);
    }
    (!extended && (1..=0x58).contains(&scancode)).then_some(scancode as u32)
}

/// Windows scan code (and whether it needs the extended flag) for an evdev
/// code, if it has one.
pub fn scancode_from_evdev(evdev: u32) -> Option<(u16, bool)> {
    if let Some(&(scancode, extended, _)) = EXTENDED.iter().find(|(_, _, e)| *e == evdev) {
        return Some((scancode, extended));
    }
    (1..=0x58).contains(&evdev).then_some((evdev as u16, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(evdev_from_scancode(0x1e, false), Some(30)); // A
        assert_eq!(evdev_from_scancode(0x1c, false), Some(28)); // Enter
        assert_eq!(evdev_from_scancode(0x1c, true), Some(96)); // keypad Enter
        assert_eq!(evdev_from_scancode(0x48, false), Some(72)); // keypad 8
        assert_eq!(evdev_from_scancode(0x48, true), Some(103)); // Up
        assert_eq!(evdev_from_scancode(0x45, true), Some(69)); // Num Lock
        assert_eq!(evdev_from_scancode(0x45, false), Some(119)); // Pause
        assert_eq!(evdev_from_scancode(0x7f, false), None);
    }

    #[test]
    fn round_trips() {
        for evdev in (1..=0x58).chain(EXTENDED.iter().map(|e| e.2)) {
            let (s, e) = scancode_from_evdev(evdev).unwrap();
            assert_eq!(evdev_from_scancode(s, e), Some(evdev), "evdev {evdev}");
        }
    }
}
