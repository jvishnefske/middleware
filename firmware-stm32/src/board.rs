//! The board profile.

use board_pins::PinMap;
use board_pins::boards::EBB42;
use middleware::SenderId;
use middleware::loader::BoardInfo;
use middleware::loader::manifest::{Link, LinkKind, Manifest};
use middleware::wasm_abi::ABI_VERSION;

/// Chip name in `sys/info`; `board_pins::boards::by_chip` maps it to
/// [`PINS`], and `mwctl` and the HIL suite match it.
pub const CHIP: &str = "stm32g0b1";

/// Firmware version in `sys/info`. `+no-wasm`: no payload runtime, so tools
/// must not offer `sys/payload/start` (mwctl reads the suffix).
pub const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

/// The pin policy (`board-pins`).
pub const PINS: &PinMap = &EBB42;

/// `sys/manifest`: no payload runtime, one serial link (USB CDC-ACM).
pub const MANIFEST: Manifest = Manifest::new(ABI_VERSION)
    .without_runtime()
    .with_link(Link {
        id: 0,
        kind: LinkKind::SERIAL,
    });

/// `sys/board`: the pin classes of [`PINS`]. No payload runtime (image and
/// memory 0), no PWM, and the host link is USB, so `uart_tx` / `uart_rx`
/// are 0 as on rp-base.
pub const fn board_info() -> BoardInfo<'static> {
    BoardInfo {
        abi_version: ABI_VERSION,
        max_image: 0,
        max_memory: 0,
        gpio: PINS.gpio_mask(),
        reserved: PINS.reserved_mask(),
        input_only: PINS.input_only_mask(),
        strapping: 0,
        adc: PINS.adc_mask(),
        pwm_max_bits: 0,
        flags: 0,
        uart_tx: 0,
        uart_rx: 0,
        pwm_pins: &[],
    }
}

/// Folds the 96-bit unique id (`UID[0..12]`: X/Y and wafer in bytes 0..4,
/// the lot number in 4..12) into the 8-byte node id: bytes 8..12 are XORed
/// into 0..4, so every UID bit reaches the id.
pub const fn node_id(uid: [u8; 12]) -> SenderId {
    let [a0, a1, a2, a3, b0, b1, b2, b3, c0, c1, c2, c3] = uid;
    SenderId([a0 ^ c0, a1 ^ c1, a2 ^ c2, a3 ^ c3, b0, b1, b2, b3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn st2_board_info_is_the_ebb42_map() {
        let b = board_info();
        assert_eq!(board_pins::boards::by_chip(CHIP), Some(PINS));
        assert_eq!(b.gpio, EBB42.gpio_mask());
        assert_eq!(b.reserved, EBB42.reserved_mask());
        assert_eq!(b.adc, 1 << 3);
        assert_eq!((b.max_image, b.max_memory, b.flags), (0, 0, 0));
        let mut buf = [0u8; 128];
        let n = b.encode(&mut buf).unwrap();
        assert_eq!(BoardInfo::decode(&buf[..n]).unwrap(), b);
    }

    #[test]
    fn st3_node_id_uses_every_uid_byte() {
        let base = node_id([0; 12]);
        for i in 0..12 {
            let mut uid = [0u8; 12];
            uid[i] = 0xa5;
            assert_ne!(node_id(uid), base, "byte {i}");
        }
    }
}
