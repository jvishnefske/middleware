//! Serial framing between the host and the USB↔ESP-NOW bridge
//! (`tools/espnow-bridge/esp8266`, an ESP8266 that is a dumb radio).
//!
//! The codec is the library's [`middleware::radio_bridge`] (H6), re-exported
//! here so the wire bytes are the ones the gateway in `mw-posix-node` and
//! the bridge firmware use; this module adds MAC text and an owned copy of
//! a received message for the blocking [`crate::air`] link.

pub use middleware::radio_bridge::*;

/// Formats a MAC as `aa:bb:cc:dd:ee:ff`.
pub fn format_mac(mac: &Mac) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Parses `aa:bb:cc:dd:ee:ff` (or with `-`); `None` if malformed.
pub fn parse_mac(s: &str) -> Option<Mac> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (dst, p) in mac.iter_mut().zip(parts) {
        if p.len() != 2 {
            return None;
        }
        *dst = u8::from_str_radix(p, 16).ok()?;
    }
    Some(mac)
}

/// The frame as sent on the wire (COBS, delimiter included), owned.
pub fn wire_vec(msg: &Msg<'_>) -> Result<Vec<u8>, BridgeError> {
    let mut out = [0u8; MAX_WIRE];
    let n = msg.encode_wire(&mut out)?;
    Ok(out.get(..n).unwrap_or_default().to_vec())
}

/// A bridge → host message, owned so it outlives the decode buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owned {
    /// `data` heard from `src`.
    Recv {
        /// Sender.
        src: Mac,
        /// The ESP-NOW frame.
        data: Vec<u8>,
    },
    /// Who the bridge is.
    Hello {
        /// The bridge's station MAC.
        mac: Mac,
        /// Its Wi-Fi channel.
        channel: u8,
        /// Its protocol version.
        version: u8,
    },
    /// The bridge's counters.
    Stats(Stats),
    /// The driver finished a send to `dst`.
    Sent {
        /// Whether it was acknowledged.
        ok: bool,
        /// Destination of that send.
        dst: Mac,
    },
    /// A host → bridge message (`Send`, `Query`) heard back.
    Other,
}

impl From<Msg<'_>> for Owned {
    fn from(m: Msg<'_>) -> Self {
        match m {
            Msg::Recv { src, data } => Owned::Recv {
                src,
                data: data.to_vec(),
            },
            Msg::Hello {
                mac,
                channel,
                version,
            } => Owned::Hello {
                mac,
                channel,
                version,
            },
            Msg::Stats(s) => Owned::Stats(s),
            Msg::Sent { ok, dst } => Owned::Sent { ok, dst },
            Msg::Send { .. } | Msg::Query => Owned::Other,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const BRIDGE: Mac = [0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54];
    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];

    #[test]
    fn o5_macs_parse_and_format() {
        assert_eq!(parse_mac("e4:b0:63:81:d3:24"), Some(DEV));
        assert_eq!(parse_mac("E4-B0-63-81-D3-24"), Some(DEV));
        assert_eq!(format_mac(&DEV), "e4:b0:63:81:d3:24");
        for bad in [
            "",
            "e4:b0:63:81:d3",
            "e4:b0:63:81:d3:24:00",
            "e4:b0:63:81:d3:2",
            "zz:b0:63:81:d3:24",
        ] {
            assert_eq!(parse_mac(bad), None, "{bad}");
        }
    }

    #[test]
    fn h6_reexport_keeps_the_wire_bytes() {
        // The golden `Recv` frame of `middleware::radio_bridge` (and of
        // `tools/espnow-bridge/esp8266/test_bridge_frame.cpp`).
        let w = wire_vec(&Msg::Recv {
            src: DEV,
            data: &[0x00, 0xff, 0x00],
        })
        .unwrap();
        let hex: String = w.iter().map(|x| format!("{x:02x}")).collect();
        assert_eq!(hex, "0802e4b06381d32402ff03fe8200");
        let mut scratch = [0u8; MAX_WIRE];
        let msg = Msg::from_run(&w[..w.len() - 1], &mut scratch).unwrap();
        assert_eq!(
            Owned::from(msg),
            Owned::Recv {
                src: DEV,
                data: vec![0x00, 0xff, 0x00]
            }
        );
        assert_eq!(Owned::from(Msg::Query), Owned::Other);
        assert_eq!(
            Owned::from(Msg::Sent { ok: true, dst: DEV }),
            Owned::Sent { ok: true, dst: DEV }
        );
    }
}
