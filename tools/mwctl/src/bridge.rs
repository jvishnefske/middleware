//! Serial framing between the host and the USB↔ESP-NOW bridge
//! (`tools/espnow-bridge/esp8266`, an ESP8266 that is a dumb radio).
//!
//! Every message is one frame: the raw bytes below, then the CRC-16
//! ([`middleware::crc::crc16`], CCITT-FALSE) of everything before it,
//! little-endian; the whole is COBS-stuffed ([`middleware::serial::cobs`])
//! and followed by one `0x00` delimiter, as on the device's own serial
//! protocol, so ROM boot text from the ESP8266 is skipped.
//!
//! | type | direction      | fields after the type byte                     |
//! |------|----------------|------------------------------------------------|
//! | 1 `Send`  | host → bridge | `dst MAC 6`, ESP-NOW frame (1..=250 bytes) |
//! | 2 `Recv`  | bridge → host | `src MAC 6`, ESP-NOW frame (1..=250 bytes) |
//! | 3 `Hello` | bridge → host | `own MAC 6`, `channel u8`, `version u8` (1) |
//! | 4 `Query` | host → bridge | none; the bridge answers `Hello` then `Stats` |
//! | 5 `Stats` | bridge → host | six u32 LE counters (see [`Stats`])        |
//! | 6 `Sent`  | bridge → host | `status u8` (0 acknowledged, 1 failed), `dst MAC 6` |
//!
//! The bridge sends `Hello` at boot. For each `Send` it adds the
//! destination as an ESP-NOW peer if needed, sends one frame and reports
//! the driver's send callback as `Sent` (for unicast, whether the peer
//! acknowledged at the MAC layer). Every frame it hears goes to the host
//! as `Recv`. Queues on the bridge are bounded and drops are counted in
//! [`Stats`].

use middleware::crc::crc16;
use middleware::serial::cobs;

/// A 48-bit ESP-NOW (Wi-Fi MAC) address.
pub type Mac = [u8; 6];

/// Bridge protocol version carried in `Hello`.
pub const VERSION: u8 = 1;
/// Largest ESP-NOW frame carried.
pub const MAX_DATA: usize = 250;
/// Largest raw frame: type, MAC, data, CRC.
pub const MAX_RAW: usize = 1 + 6 + MAX_DATA + 2;
/// Largest frame on the wire, delimiter included.
pub const MAX_WIRE: usize = cobs::max_encoded_len(MAX_RAW) + 1;
/// Serial speed of the bridge firmware.
pub const BAUD: u32 = 460_800;

const T_SEND: u8 = 1;
const T_RECV: u8 = 2;
const T_HELLO: u8 = 3;
const T_QUERY: u8 = 4;
const T_STATS: u8 = 5;
const T_SENT: u8 = 6;

/// The bridge's counters (all saturating on the bridge).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frames heard and forwarded to the host.
    pub rx: u32,
    /// Frames heard but dropped (receive queue full or too long).
    pub rx_dropped: u32,
    /// Sends the driver reported acknowledged.
    pub tx_ok: u32,
    /// Sends the driver reported failed (no MAC-level acknowledgement).
    pub tx_fail: u32,
    /// `Send` frames dropped (queue full, peer table error).
    pub tx_dropped: u32,
    /// Serial frames with a bad COBS encoding, CRC or layout (including a
    /// `Send` whose data is not 1..=250 bytes).
    pub serial_bad: u32,
}

/// One bridge message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// Host → bridge: send `data` to `dst`.
    Send {
        /// Destination (a station, or `FF:FF:FF:FF:FF:FF`).
        dst: Mac,
        /// The ESP-NOW frame.
        data: Vec<u8>,
    },
    /// Bridge → host: `data` heard from `src`.
    Recv {
        /// Sender.
        src: Mac,
        /// The ESP-NOW frame.
        data: Vec<u8>,
    },
    /// Bridge → host: who the bridge is.
    Hello {
        /// The bridge's station MAC.
        mac: Mac,
        /// Its Wi-Fi channel.
        channel: u8,
        /// Its protocol version.
        version: u8,
    },
    /// Host → bridge: ask for `Hello` and `Stats`.
    Query,
    /// Bridge → host: counters.
    Stats(Stats),
    /// Bridge → host: the driver finished a send to `dst`.
    Sent {
        /// Whether it was acknowledged.
        ok: bool,
        /// Destination of that send.
        dst: Mac,
    },
}

/// Why bytes are not a bridge frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    /// Bad COBS encoding.
    Cobs,
    /// CRC mismatch.
    Crc,
    /// Wrong length for its type, or data outside 1..=250 bytes.
    Malformed,
    /// Unknown type byte.
    UnknownType(u8),
}

fn mac_at(b: &[u8]) -> Option<Mac> {
    b.get(..6)?.try_into().ok()
}

impl Msg {
    /// The raw frame (type, fields, CRC) before COBS.
    pub fn raw(&self) -> Result<Vec<u8>, BridgeError> {
        let mut out = Vec::with_capacity(MAX_RAW);
        match self {
            Msg::Send { dst: mac, data } | Msg::Recv { src: mac, data } => {
                if data.is_empty() || data.len() > MAX_DATA {
                    return Err(BridgeError::Malformed);
                }
                out.push(if matches!(self, Msg::Send { .. }) {
                    T_SEND
                } else {
                    T_RECV
                });
                out.extend_from_slice(mac);
                out.extend_from_slice(data);
            }
            Msg::Hello {
                mac,
                channel,
                version,
            } => {
                out.push(T_HELLO);
                out.extend_from_slice(mac);
                out.extend_from_slice(&[*channel, *version]);
            }
            Msg::Query => out.push(T_QUERY),
            Msg::Stats(s) => {
                out.push(T_STATS);
                for v in [
                    s.rx,
                    s.rx_dropped,
                    s.tx_ok,
                    s.tx_fail,
                    s.tx_dropped,
                    s.serial_bad,
                ] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            Msg::Sent { ok, dst } => {
                out.push(T_SENT);
                out.push(u8::from(!ok));
                out.extend_from_slice(dst);
            }
        }
        let crc = crc16(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    /// The frame as sent on the wire: COBS-stuffed raw bytes and a `0x00`.
    pub fn wire(&self) -> Result<Vec<u8>, BridgeError> {
        let raw = self.raw()?;
        let mut out = vec![0u8; cobs::max_encoded_len(raw.len())];
        let n = cobs::encode(&raw, &mut out).ok_or(BridgeError::Cobs)?;
        out.truncate(n);
        out.push(0);
        Ok(out)
    }

    /// Parses a raw frame (COBS already removed).
    pub fn from_raw(raw: &[u8]) -> Result<Msg, BridgeError> {
        let (body, crc) = raw.split_last_chunk::<2>().ok_or(BridgeError::Malformed)?;
        if crc16(body) != u16::from_le_bytes(*crc) {
            return Err(BridgeError::Crc);
        }
        let (&t, rest) = body.split_first().ok_or(BridgeError::Malformed)?;
        let bad = BridgeError::Malformed;
        match t {
            T_SEND | T_RECV => {
                let mac = mac_at(rest).ok_or(bad)?;
                let data = rest.get(6..).ok_or(bad)?;
                if data.is_empty() || data.len() > MAX_DATA {
                    return Err(bad);
                }
                let data = data.to_vec();
                Ok(if t == T_SEND {
                    Msg::Send { dst: mac, data }
                } else {
                    Msg::Recv { src: mac, data }
                })
            }
            T_HELLO => match rest {
                [m0, m1, m2, m3, m4, m5, channel, version] => Ok(Msg::Hello {
                    mac: [*m0, *m1, *m2, *m3, *m4, *m5],
                    channel: *channel,
                    version: *version,
                }),
                _ => Err(bad),
            },
            T_QUERY if rest.is_empty() => Ok(Msg::Query),
            T_STATS if rest.len() == 24 => {
                let v = |i: usize| {
                    rest.get(i * 4..i * 4 + 4)
                        .and_then(|b| b.try_into().ok())
                        .map_or(0, u32::from_le_bytes)
                };
                Ok(Msg::Stats(Stats {
                    rx: v(0),
                    rx_dropped: v(1),
                    tx_ok: v(2),
                    tx_fail: v(3),
                    tx_dropped: v(4),
                    serial_bad: v(5),
                }))
            }
            T_SENT => match rest {
                [status, m0, m1, m2, m3, m4, m5] if *status <= 1 => Ok(Msg::Sent {
                    ok: *status == 0,
                    dst: [*m0, *m1, *m2, *m3, *m4, *m5],
                }),
                _ => Err(bad),
            },
            T_QUERY | T_STATS => Err(bad),
            other => Err(BridgeError::UnknownType(other)),
        }
    }

    /// COBS-decodes one delimited run (delimiter removed) and parses it.
    pub fn from_run(run: &[u8]) -> Result<Msg, BridgeError> {
        let mut buf = run.to_vec();
        let n = cobs::decode_in_place(&mut buf).ok_or(BridgeError::Cobs)?;
        Msg::from_raw(buf.get(..n).ok_or(BridgeError::Cobs)?)
    }
}

/// Decodes one delimited run, resynchronising past leading garbage (the
/// ESP8266 ROM prints its boot banner at 74880 baud, read here as
/// arbitrary non-zero bytes glued to the first frame): tries the whole run,
/// then each shorter suffix, and returns the number of bytes skipped with
/// the first message that decodes.
pub fn decode_run(run: &[u8]) -> Option<(usize, Msg)> {
    // Bounded by the run length.
    (0..run.len()).find_map(|skip| Msg::from_run(run.get(skip..)?).ok().map(|m| (skip, m)))
}

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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use middleware::serial::frame::{Split, Splitter};

    pub(crate) const BRIDGE: Mac = [0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54];
    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];

    /// Hex of a byte string.
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The golden frames `tools/espnow-bridge/esp8266/test_bridge_frame.cpp`
    /// checks the C++ codec against (wire bytes, delimiter included); also
    /// reproduced by an independent Python COBS/CRC-16 script.
    pub(crate) fn golden() -> Vec<(Msg, &'static str)> {
        vec![
            (
                Msg::Send {
                    dst: DEV,
                    data: b"MR\x01\x01\x00\x00".to_vec(),
                },
                "0c01e4b06381d3244d520101010398c400",
            ),
            (
                Msg::Recv {
                    src: DEV,
                    data: vec![0x00, 0xff, 0x00],
                },
                "0802e4b06381d32402ff03fe8200",
            ),
            (
                Msg::Hello {
                    mac: BRIDGE,
                    channel: 1,
                    version: VERSION,
                },
                "0c035ccf7f72f654010149d300",
            ),
            (Msg::Query, "040474a100"),
            (
                Msg::Stats(Stats {
                    rx: 1,
                    rx_dropped: 2,
                    tx_ok: 3,
                    tx_fail: 4,
                    tx_dropped: 5,
                    serial_bad: 0x0102_0304,
                }),
                "030501010102020101020301010204010102050101070403020124d100",
            ),
            (
                Msg::Sent {
                    ok: false,
                    dst: DEV,
                },
                "0b0601e4b06381d324c54000",
            ),
            (Msg::Sent { ok: true, dst: DEV }, "020609e4b06381d324a4f800"),
        ]
    }

    #[test]
    fn o5_messages_round_trip_through_the_wire() {
        for (msg, _) in golden() {
            let wire = msg.wire().unwrap();
            assert_eq!(wire.last(), Some(&0));
            assert!(!wire[..wire.len() - 1].contains(&0));
            assert_eq!(Msg::from_run(&wire[..wire.len() - 1]), Ok(msg));
        }
        let big = Msg::Send {
            dst: DEV,
            data: vec![0; MAX_DATA],
        };
        let wire = big.wire().unwrap();
        assert!(wire.len() <= MAX_WIRE);
        assert_eq!(Msg::from_run(&wire[..wire.len() - 1]), Ok(big));
    }

    #[test]
    fn o5_golden_frames_match_the_bridge_firmware() {
        for (msg, want) in golden() {
            assert_eq!(hex(&msg.wire().unwrap()), want, "{msg:?}");
        }
    }

    #[test]
    fn o5_bad_frames_are_rejected() {
        assert_eq!(
            Msg::Send {
                dst: DEV,
                data: vec![]
            }
            .raw(),
            Err(BridgeError::Malformed)
        );
        assert_eq!(
            Msg::Recv {
                src: DEV,
                data: vec![0; MAX_DATA + 1]
            }
            .raw(),
            Err(BridgeError::Malformed)
        );
        let mut raw = Msg::Query.raw().unwrap();
        raw[0] ^= 1;
        assert_eq!(Msg::from_raw(&raw), Err(BridgeError::Crc));
        let with_crc = |b: &[u8]| {
            let mut v = b.to_vec();
            v.extend_from_slice(&crc16(b).to_le_bytes());
            v
        };
        assert_eq!(
            Msg::from_raw(&with_crc(&[9])),
            Err(BridgeError::UnknownType(9))
        );
        assert_eq!(
            Msg::from_raw(&with_crc(&[T_QUERY, 0])),
            Err(BridgeError::Malformed)
        );
        assert_eq!(
            Msg::from_raw(&with_crc(&[T_SEND, 1, 2, 3, 4, 5, 6])),
            Err(BridgeError::Malformed)
        );
        assert_eq!(
            Msg::from_raw(&with_crc(&[T_SENT, 2, 1, 2, 3, 4, 5, 6])),
            Err(BridgeError::Malformed)
        );
        assert_eq!(
            Msg::from_raw(&with_crc(&[T_STATS, 0])),
            Err(BridgeError::Malformed)
        );
        assert_eq!(Msg::from_raw(&[1]), Err(BridgeError::Malformed));
        assert_eq!(Msg::from_run(&[0x05, 1, 0]), Err(BridgeError::Cobs));
    }

    #[test]
    fn o5_frames_split_out_of_a_noisy_stream() {
        let mut stream = b"\r\n ets Jan  8 2013,rst cause:2\r\n".to_vec();
        stream.extend(
            Msg::Hello {
                mac: BRIDGE,
                channel: 1,
                version: 1,
            }
            .wire()
            .unwrap(),
        );
        stream.extend(Msg::Sent { ok: true, dst: DEV }.wire().unwrap());
        let mut s: Splitter<MAX_WIRE> = Splitter::new();
        let mut got = Vec::new();
        for b in stream {
            if let Split::Frame(_) = s.push(b) {
                got.push(decode_run(s.frame()));
            }
        }
        // The banner has no delimiter, so it shares a run with the first
        // frame; the frame is still found behind it.
        assert_eq!(
            got,
            [
                Some((
                    32,
                    Msg::Hello {
                        mac: BRIDGE,
                        channel: 1,
                        version: 1
                    }
                )),
                Some((0, Msg::Sent { ok: true, dst: DEV }))
            ]
        );
        assert_eq!(decode_run(b"noise"), None);
    }

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
}
