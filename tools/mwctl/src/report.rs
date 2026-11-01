//! Human-readable output.
//!
//! * [`format_info`] — `sys/info` as aligned `name: value` lines, noting a
//!   protocol-only (`+no-wasm`) firmware and an ABI mismatch.
//! * [`format_trust`] — `sys/trust/info`: the state, the last signed
//!   version and the pinned key in hex.
//! * [`format_board`] — `sys/board`: limits, flags and each pin class as a
//!   GPIO list, then the PWM table.
//! * [`format_event`] — one line per publication: `sys/log` as text,
//!   `sys/payload/status` by name, anything else as UTF-8 or hex; noise runs
//!   as their length and printable characters.
//! * [`format_fw_info`] — `sys/fw/info`: update state, running slot, each
//!   slot's version and TBYB flag, the anti-rollback floor and the update
//!   in progress.
//! * [`status_name`] — the payload status names used by the web page.

use crate::device::Info;
use crate::link::Event;
use middleware::SenderId;
use middleware::loader::trust::{TrustCode, TrustInfo, TrustState};
use middleware::loader::{
    BOARD_PWM_REF_TICK, BOARD_RUNTIME, BOARD_USB_JTAG, BoardInfo, PayloadStatus, keys,
};
use middleware::wasm_abi::ABI_VERSION;
use std::fmt::Write as _;

/// The name of a payload status.
pub const fn status_name(s: PayloadStatus) -> &'static str {
    match s {
        PayloadStatus::Empty => "empty",
        PayloadStatus::Stopped => "stopped",
        PayloadStatus::Running => "running",
        PayloadStatus::Trapped => "trapped",
        PayloadStatus::Loading => "loading",
    }
}

/// `sys/info` for printing.
pub fn format_info(i: &Info) -> String {
    let abi = if i.abi_version == ABI_VERSION {
        format!("v{}", i.abi_version)
    } else {
        format!("v{} (mwctl expects v{ABI_VERSION})", i.abi_version)
    };
    let runtime = if i.runs_payloads() {
        "WASM (payloads run)"
    } else {
        "none: protocol only, payloads are stored but not run"
    };
    let node = match i.node {
        Some(SenderId(b)) => format!(
            "node:             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\n",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
        ),
        None => String::new(),
    };
    format!(
        "chip:             {}\nfirmware:         {}\n{node}runtime:          {runtime}\nABI:              {abi}\npayload capacity: {} bytes\npayload status:   {}\n",
        i.chip,
        i.firmware,
        i.capacity,
        status_name(i.status)
    )
}

fn pin_list(mask: u64) -> String {
    let pins: Vec<String> = (0..64).filter(|n| mask >> n & 1 == 1).map(|n: u32| n.to_string()).collect();
    if pins.is_empty() { "-".into() } else { pins.join(" ") }
}

/// `sys/board` for printing.
pub fn format_board(b: &BoardInfo<'_>) -> String {
    let yes = |f: u8| if b.flags & f != 0 { "yes" } else { "no" };
    let mut s = format!(
        "ABI:              v{}\nmax image:        {} bytes\nmax memory:       {} bytes\nruntime:          {}\nUSB-Serial-JTAG:  {}\nconsole UART:     tx {} rx {}\n",
        b.abi_version,
        b.max_image,
        b.max_memory,
        yes(BOARD_RUNTIME),
        yes(BOARD_USB_JTAG),
        b.uart_tx,
        b.uart_rx,
    );
    for (name, mask) in [
        ("gpio", b.gpio),
        ("reserved", b.reserved),
        ("input only", b.input_only),
        ("strapping", b.strapping),
        ("ADC1", b.adc),
    ] {
        let _ = writeln!(s, "{:<18}{}", format!("{name}:"), pin_list(mask));
    }
    let _ = write!(s, "PWM:              {} bits, REF_TICK {}", b.pwm_max_bits, yes(BOARD_PWM_REF_TICK));
    for (i, pin) in b.pwm_pins.iter().enumerate() {
        let _ = write!(s, "{}({},{})={pin}", if i == 0 { "; " } else { " " }, i / 2, i % 2);
    }
    s.push('\n');
    s
}

/// `sys/fw/info` for printing.
pub fn format_fw_info(i: &middleware::fw_ota::FwInfo) -> String {
    use crate::fw::format_version;
    use middleware::fw_ota::Slot;
    let slot = |s: Slot| {
        let info = i.slots[s as usize];
        let what = match info.version {
            None => "empty".to_owned(),
            Some(v) if info.tbyb => format!("{} (TBYB, not bought)", format_version(v)),
            Some(v) => format_version(v),
        };
        let running = if i.running == s { " <- running" } else { "" };
        format!("slot {}:           {what}{running}\n", s.name())
    };
    let target = match i.target {
        Some(t) => format!(
            "update:           {} {} into slot {}{}\n",
            i.state.name(),
            format_version(i.version),
            t.name(),
            if i.len > 0 { format!(", {}/{} bytes", i.received, i.len) } else { String::new() }
        ),
        None => format!("update:           {}\n", i.state.name()),
    };
    let b = i.node.0;
    format!(
        "{}{}{target}min version:      {} (a new image must be newer)\nself-test:        {}\nnode:             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\n",
        slot(Slot::A),
        slot(Slot::B),
        format_version(i.min_version),
        if i.self_test_ok { "passed" } else { "not passed" },
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
    )
}

/// `sys/trust/info` for printing.
pub fn format_trust(t: &TrustInfo) -> String {
    let state = match t.code() {
        TrustCode::Unclaimed => "unclaimed (any upload is accepted; the first claim pins a key)",
        TrustCode::Claimed => "claimed",
        TrustCode::RekeyOpen => "claimed, re-key window open (a new claim is accepted)",
    };
    match t.state {
        TrustState::Unclaimed => format!("trust:            {state}\n"),
        TrustState::Claimed { key, min_version } => format!(
            "trust:            {state}\nlast version:     {}\npinned key:       {}\n",
            min_version.0,
            crate::sign::hex(&key.0)
        ),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (n, b) in bytes.iter().enumerate() {
        if n > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn printable(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '.'
            }
        })
        .collect()
}

/// One line describing `e`.
pub fn format_event(e: &Event) -> String {
    match e {
        Event::Publish { key, payload } if key == keys::LOG.as_str() => {
            // Control characters (an ANSI escape from a radio-supplied
            // name, say) are shown escaped, never sent to the terminal.
            let text = String::from_utf8_lossy(payload);
            let line: String = text
                .trim_end()
                .chars()
                .flat_map(|c| {
                    let escaped = c.is_control().then(|| c.escape_default());
                    escaped.into_iter().flatten().chain((!c.is_control()).then_some(c))
                })
                .collect();
            format!("[log] {line}")
        }
        Event::Publish { key, payload } if key == keys::STATUS.as_str() => {
            match payload.as_slice() {
                [b] => match PayloadStatus::from_byte(*b) {
                    Some(s) => format!("[status] {}", status_name(s)),
                    None => format!("[status] unknown ({b})"),
                },
                other => format!("[status] malformed ({})", hex(other)),
            }
        }
        Event::Publish { key, payload } => match std::str::from_utf8(payload) {
            Ok(text) if !text.chars().any(char::is_control) => format!("[pub] {key}: {text}"),
            _ => format!("[pub] {key}: {}", hex(payload)),
        },
        Event::Noise(bytes) => format!("[noise] {} byte(s): {}", bytes.len(), printable(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_notes_protocol_only_firmware() {
        let i = Info {
            abi_version: ABI_VERSION,
            capacity: 65536,
            status: PayloadStatus::Stopped,
            chip: "esp8266".into(),
            firmware: "0.1.0+no-wasm".into(),
            node: None,
        };
        let s = format_info(&i);
        assert!(!s.contains("node:"), "{s}");
        assert!(s.contains("chip:             esp8266\n"), "{s}");
        assert!(s.contains("protocol only"), "{s}");
        assert!(s.contains("65536 bytes"), "{s}");
        assert!(s.contains("stopped"), "{s}");
        let s = format_info(&Info {
            firmware: "0.1.0".into(),
            abi_version: ABI_VERSION + 1,
            ..i.clone()
        });
        assert!(s.contains("WASM (payloads run)"), "{s}");
        assert!(s.contains("mwctl expects"), "{s}");
        let s = format_info(&Info {
            node: Some(SenderId::from_mac([0x24, 0x6f, 0x28, 0xaa, 0xbb, 0xcc])),
            ..i
        });
        assert!(
            s.contains("firmware:         0.1.0+no-wasm\nnode:             246f28aabbcc0000\n"),
            "{s}"
        );
    }

    #[test]
    fn board_lists_pin_classes_and_pwm_table() {
        let b = BoardInfo {
            abi_version: 3,
            max_image: 81920,
            max_memory: 131072,
            gpio: 0b1111,
            reserved: 0b10,
            input_only: 0,
            strapping: 0b1,
            adc: 0,
            pwm_max_bits: 14,
            flags: BOARD_RUNTIME | BOARD_USB_JTAG,
            uart_tx: 43,
            uart_rx: 44,
            pwm_pins: &[2, 3],
        };
        let s = format_board(&b);
        assert!(s.contains("gpio:             0 1 2 3\n"), "{s}");
        assert!(s.contains("strapping:        0\n"), "{s}");
        assert!(s.contains("input only:       -\n"), "{s}");
        assert!(s.contains("USB-Serial-JTAG:  yes"), "{s}");
        assert!(s.contains("14 bits, REF_TICK no; (0,0)=2 (0,1)=3\n"), "{s}");
    }

    #[test]
    fn trust_info_shows_state_version_and_key() {
        use middleware::loader::trust::{PublicKey, Version};
        let s = format_trust(&TrustState::Unclaimed.info(false));
        assert!(s.starts_with("trust:            unclaimed"), "{s}");
        let claimed = TrustState::Claimed {
            key: PublicKey([0xab; 32]),
            min_version: Version(42),
        };
        let s = format_trust(&claimed.info(false));
        assert!(s.contains("trust:            claimed\n"), "{s}");
        assert!(s.contains("last version:     42"), "{s}");
        assert!(s.contains(&"ab".repeat(32)), "{s}");
        assert!(format_trust(&claimed.info(true)).contains("re-key window open"));
    }

    #[test]
    fn y9_fw_info_names_slots_versions_and_progress() {
        use middleware::fw_ota::wire::{FwInfo, SlotInfo, StateCode};
        use middleware::fw_ota::Slot;
        use middleware::loader::trust::Version;
        let i = FwInfo {
            state: StateCode::Receiving,
            running: Slot::A,
            slots: [SlotInfo { version: Some(Version(0x0001_0000)), tbyb: false }, SlotInfo::default()],
            min_version: Version(0x0001_0000),
            node: SenderId([1, 2, 3, 4, 5, 6, 7, 8]),
            target: Some(Slot::B),
            version: Version(0x0001_0001),
            len: 2048,
            received: 512,
            self_test_ok: true,
        };
        let s = format_fw_info(&i);
        assert!(s.contains("slot A:           1.0 <- running\n"), "{s}");
        assert!(s.contains("slot B:           empty\n"), "{s}");
        assert!(s.contains("update:           receiving 1.1 into slot B, 512/2048 bytes\n"), "{s}");
        assert!(s.contains("node:             0102030405060708"), "{s}");
        let t = FwInfo {
            state: StateCode::Trial,
            running: Slot::B,
            slots: [i.slots[0], SlotInfo { version: Some(Version(0x0001_0001)), tbyb: true }],
            len: 0,
            target: None,
            self_test_ok: false,
            ..i
        };
        let s = format_fw_info(&t);
        assert!(s.contains("slot B:           1.1 (TBYB, not bought) <- running"), "{s}");
        assert!(s.contains("update:           trial\n") && s.contains("not passed"), "{s}");
    }

    #[test]
    fn events_are_one_readable_line() {
        let p = |k: &str, v: &[u8]| Event::Publish {
            key: k.into(),
            payload: v.to_vec(),
        };
        assert_eq!(format_event(&p("sys/log", b"hello\n")), "[log] hello");
        assert_eq!(
            format_event(&p("sys/log", b"disc: up x\x1b[2Jy\rz\n")),
            "[log] disc: up x\\u{1b}[2Jy\\rz"
        );
        assert_eq!(format_event(&p("sys/log", "caf\u{e9}".as_bytes())), "[log] caf\u{e9}");
        assert_eq!(
            format_event(&p("sys/payload/status", &[2])),
            "[status] running"
        );
        assert_eq!(
            format_event(&p("sys/payload/status", &[9])),
            "[status] unknown (9)"
        );
        assert_eq!(format_event(&p("app/t", b"21.5")), "[pub] app/t: 21.5");
        assert_eq!(format_event(&p("app/t", &[0, 255])), "[pub] app/t: 00 ff");
        assert_eq!(
            format_event(&Event::Noise(b"ets\x01Jan".to_vec())),
            "[noise] 7 byte(s): ets.Jan"
        );
    }
}
