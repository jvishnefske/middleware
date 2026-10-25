//! Human-readable output.
//!
//! * [`format_info`] — `sys/info` as aligned `name: value` lines, noting a
//!   protocol-only (`+no-wasm`) firmware and an ABI mismatch.
//! * [`format_event`] — one line per publication: `sys/log` as text,
//!   `sys/payload/status` by name, anything else as UTF-8 or hex; noise runs
//!   as their length and printable characters.
//! * [`status_name`] — the payload status names used by the web page.

use crate::device::Info;
use crate::link::Event;
use middleware::loader::{PayloadStatus, keys};
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
    format!(
        "chip:             {}\nfirmware:         {}\nruntime:          {runtime}\nABI:              {abi}\npayload capacity: {} bytes\npayload status:   {}\n",
        i.chip,
        i.firmware,
        i.capacity,
        status_name(i.status)
    )
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
            format!("[log] {}", String::from_utf8_lossy(payload).trim_end())
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
        };
        let s = format_info(&i);
        assert!(s.contains("chip:             esp8266\n"), "{s}");
        assert!(s.contains("protocol only"), "{s}");
        assert!(s.contains("65536 bytes"), "{s}");
        assert!(s.contains("stopped"), "{s}");
        let s = format_info(&Info {
            firmware: "0.1.0".into(),
            abi_version: ABI_VERSION + 1,
            ..i
        });
        assert!(s.contains("WASM (payloads run)"), "{s}");
        assert!(s.contains("mwctl expects"), "{s}");
    }

    #[test]
    fn events_are_one_readable_line() {
        let p = |k: &str, v: &[u8]| Event::Publish {
            key: k.into(),
            payload: v.to_vec(),
        };
        assert_eq!(format_event(&p("sys/log", b"hello\n")), "[log] hello");
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
