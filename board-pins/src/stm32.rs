//! STM32 pin numbers.
//!
//! A payload pin number is `port · 16 + line`: PA0 = 0, PA15 = 15, PB0 = 16,
//! PC0 = 32, PD0 = 48, PD15 = 63. Ports E and up (PF0/PF1 are the
//! oscillator, PF2 is NRST on the STM32G0B1 in LQFP48) are past the ABI's 64
//! pin numbers and so never payload pins.

/// GPIO ports that have payload pin numbers.
pub const PORTS: [char; 4] = ['A', 'B', 'C', 'D'];

/// The pin number of port `port` (`'A'`..=`'D'`) line `line` (0–15).
pub const fn pin(port: char, line: u8) -> Option<u8> {
    let p = match port {
        'A' => 0,
        'B' => 1,
        'C' => 2,
        'D' => 3,
        _ => return None,
    };
    if line < 16 { Some(p * 16 + line) } else { None }
}

/// The port and line of a pin number: `(port, line)`, or `None` past 63.
pub const fn port_line(pin: u8) -> Option<(char, u8)> {
    if pin >= 64 {
        return None;
    }
    Some((PORTS[(pin / 16) as usize], pin % 16))
}

/// `pin('A', n)` for a constant line; out-of-range lines fail the build.
pub(crate) const fn pa(line: u8) -> u8 {
    assert!(line < 16);
    line
}

/// `pin('B', n)`.
pub(crate) const fn pb(line: u8) -> u8 {
    assert!(line < 16);
    16 + line
}

/// `pin('C', n)`.
pub(crate) const fn pc(line: u8) -> u8 {
    assert!(line < 16);
    32 + line
}

/// `pin('D', n)`.
pub(crate) const fn pd(line: u8) -> u8 {
    assert!(line < 16);
    48 + line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_round_trip_for_every_pin() {
        for n in 0..64u8 {
            let (port, line) = port_line(n).unwrap();
            assert_eq!(pin(port, line), Some(n));
        }
        assert_eq!(port_line(64), None);
        assert_eq!(pin('E', 0), None);
        assert_eq!(pin('A', 16), None);
        assert_eq!((pa(3), pb(6), pc(13), pd(2)), (3, 22, 45, 50));
    }
}
