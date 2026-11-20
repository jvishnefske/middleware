//! Serial framing between a host and the USB↔ESP-NOW bridge
//! (`tools/espnow-bridge/esp8266`, an ESP8266 that is a dumb radio).
//!
//! Always built, `no_std` and heap-free: [`Msg`] borrows its data and
//! encoding writes into a caller's buffer, so the same codec serves `mwctl`
//! (std), the gateway in `mw-posix-node` ([`crate::espnow::bridge`], feature
//! `espnow`) and any microcontroller host. EG1, EG2 (moved from `mwctl`, O5).
//!
//! Every message is one frame: the raw bytes below, then the CRC-16
//! ([`crate::crc::crc16`], CCITT-FALSE) of everything before it,
//! little-endian; the whole is COBS-stuffed ([`crate::serial::cobs`])
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

use crate::crc::crc16;
use crate::serial::cobs;

/// A 48-bit ESP-NOW (Wi-Fi MAC) address.
pub type Mac = [u8; 6];

/// The ESP-NOW broadcast address.
pub const BROADCAST: Mac = [0xff; 6];
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

/// Type byte of `Send`.
pub const T_SEND: u8 = 1;
/// Type byte of `Recv`.
pub const T_RECV: u8 = 2;
/// Type byte of `Hello`.
pub const T_HELLO: u8 = 3;
/// Type byte of `Query`.
pub const T_QUERY: u8 = 4;
/// Type byte of `Stats`.
pub const T_STATS: u8 = 5;
/// Type byte of `Sent`.
pub const T_SENT: u8 = 6;

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

/// One bridge message; `Send` and `Recv` borrow their ESP-NOW frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg<'a> {
    /// Host → bridge: send `data` to `dst`.
    Send {
        /// Destination (a station, or [`BROADCAST`]).
        dst: Mac,
        /// The ESP-NOW frame.
        data: &'a [u8],
    },
    /// Bridge → host: `data` heard from `src`.
    Recv {
        /// Sender.
        src: Mac,
        /// The ESP-NOW frame.
        data: &'a [u8],
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

/// Why bytes are not a bridge frame, or a frame does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    /// Bad COBS encoding (or a run longer than [`MAX_WIRE`]).
    Cobs,
    /// CRC mismatch.
    Crc,
    /// Wrong length for its type, or data outside 1..=250 bytes.
    Malformed,
    /// Unknown type byte.
    UnknownType(u8),
    /// The output buffer is too small for the encoded frame.
    BufferTooSmall,
}

impl core::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BridgeError::Cobs => f.write_str("bad COBS encoding"),
            BridgeError::Crc => f.write_str("CRC mismatch"),
            BridgeError::Malformed => f.write_str("malformed bridge frame"),
            BridgeError::UnknownType(t) => write!(f, "unknown bridge frame type {t}"),
            BridgeError::BufferTooSmall => f.write_str("output buffer too small"),
        }
    }
}

/// A bounded writer over a caller's buffer.
struct Cursor<'b> {
    out: &'b mut [u8],
    len: usize,
}

impl Cursor<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), BridgeError> {
        let end = self.len + bytes.len();
        self.out
            .get_mut(self.len..end)
            .ok_or(BridgeError::BufferTooSmall)?
            .copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

fn mac_at(b: &[u8]) -> Option<Mac> {
    b.get(..6)?.try_into().ok()
}

impl<'a> Msg<'a> {
    /// Writes the raw frame (type, fields, CRC) before COBS into `out` and
    /// returns its length.
    pub fn encode_raw(&self, out: &mut [u8]) -> Result<usize, BridgeError> {
        let mut c = Cursor { out, len: 0 };
        match self {
            Msg::Send { dst: mac, data } | Msg::Recv { src: mac, data } => {
                if data.is_empty() || data.len() > MAX_DATA {
                    return Err(BridgeError::Malformed);
                }
                let t = if matches!(self, Msg::Send { .. }) {
                    T_SEND
                } else {
                    T_RECV
                };
                c.put(&[t])?;
                c.put(mac)?;
                c.put(data)?;
            }
            Msg::Hello {
                mac,
                channel,
                version,
            } => {
                c.put(&[T_HELLO])?;
                c.put(mac)?;
                c.put(&[*channel, *version])?;
            }
            Msg::Query => c.put(&[T_QUERY])?,
            Msg::Stats(s) => {
                c.put(&[T_STATS])?;
                for v in [
                    s.rx,
                    s.rx_dropped,
                    s.tx_ok,
                    s.tx_fail,
                    s.tx_dropped,
                    s.serial_bad,
                ] {
                    c.put(&v.to_le_bytes())?;
                }
            }
            Msg::Sent { ok, dst } => {
                c.put(&[T_SENT, u8::from(!ok)])?;
                c.put(dst)?;
            }
        }
        let n = c.len;
        let crc = crc16(c.out.get(..n).unwrap_or_default());
        c.put(&crc.to_le_bytes())?;
        Ok(c.len)
    }

    /// Writes the frame as sent on the wire (COBS-stuffed raw bytes and a
    /// `0x00`) into `out` and returns its length. At most [`MAX_WIRE`].
    pub fn encode_wire(&self, out: &mut [u8]) -> Result<usize, BridgeError> {
        let mut raw = [0u8; MAX_RAW];
        let n = self.encode_raw(&mut raw)?;
        let raw = raw.get(..n).ok_or(BridgeError::Malformed)?;
        let m = cobs::encode(raw, out).ok_or(BridgeError::BufferTooSmall)?;
        *out.get_mut(m).ok_or(BridgeError::BufferTooSmall)? = 0;
        Ok(m + 1)
    }

    /// Parses a raw frame (COBS already removed).
    pub fn from_raw(raw: &'a [u8]) -> Result<Msg<'a>, BridgeError> {
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

    /// COBS-decodes one delimited run (delimiter removed) through `scratch`
    /// and parses it; the message borrows `scratch`.
    pub fn from_run(run: &[u8], scratch: &'a mut [u8]) -> Result<Msg<'a>, BridgeError> {
        let n = decode_into(run, scratch).ok_or(BridgeError::Cobs)?;
        Msg::from_raw(scratch.get(..n).ok_or(BridgeError::Cobs)?)
    }
}

/// Copies `run` into `scratch` and COBS-decodes it there; the decoded
/// length, or `None` if it does not fit or is not COBS.
fn decode_into(run: &[u8], scratch: &mut [u8]) -> Option<usize> {
    let buf = scratch.get_mut(..run.len())?;
    buf.copy_from_slice(run);
    cobs::decode_in_place(buf)
}

/// Decodes one delimited run, resynchronising past leading garbage (the
/// ESP8266 ROM prints its boot banner at 74880 baud, read here as
/// arbitrary non-zero bytes glued to the first frame): tries the whole run,
/// then each shorter suffix, and returns the number of bytes skipped with
/// the first message that decodes. `None` if no suffix decodes.
pub fn decode_run<'a>(run: &[u8], scratch: &'a mut [u8; MAX_WIRE]) -> Option<(usize, Msg<'a>)> {
    let mut found = None;
    // Bounded by the run length.
    for skip in 0..run.len() {
        let Some(tail) = run.get(skip..) else { break };
        let Some(n) = decode_into(tail, scratch) else {
            continue;
        };
        if scratch
            .get(..n)
            .is_some_and(|raw| Msg::from_raw(raw).is_ok())
        {
            found = Some((skip, n));
            break;
        }
    }
    // The loop stops on the first success, so `scratch` still holds it.
    let (skip, n) = found?;
    let msg = Msg::from_raw(scratch.get(..n)?).ok()?;
    Some((skip, msg))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::serial::frame::{Split, Splitter};
    use std::format;
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    const BRIDGE: Mac = [0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54];
    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];

    /// Hex of a byte string.
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn wire(msg: &Msg<'_>) -> Vec<u8> {
        let mut out = [0u8; MAX_WIRE];
        let n = msg.encode_wire(&mut out).unwrap();
        out[..n].to_vec()
    }

    fn raw(msg: &Msg<'_>) -> Result<Vec<u8>, BridgeError> {
        let mut out = [0u8; MAX_RAW];
        let n = msg.encode_raw(&mut out)?;
        Ok(out[..n].to_vec())
    }

    const SEND_DATA: &[u8] = b"MR\x01\x01\x00\x00";
    const RECV_DATA: &[u8] = &[0x00, 0xff, 0x00];

    /// The golden frames `tools/espnow-bridge/esp8266/test_bridge_frame.cpp`
    /// checks the C++ codec against (wire bytes, delimiter included); also
    /// reproduced by an independent Python COBS/CRC-16 script.
    fn golden() -> Vec<(Msg<'static>, &'static str)> {
        vec![
            (
                Msg::Send {
                    dst: DEV,
                    data: SEND_DATA,
                },
                "0c01e4b06381d3244d520101010398c400",
            ),
            (
                Msg::Recv {
                    src: DEV,
                    data: RECV_DATA,
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
            let w = wire(&msg);
            assert_eq!(w.last(), Some(&0));
            assert!(!w[..w.len() - 1].contains(&0));
            let mut scratch = [0u8; MAX_WIRE];
            assert_eq!(Msg::from_run(&w[..w.len() - 1], &mut scratch), Ok(msg));
        }
        let zeros = [0u8; MAX_DATA];
        let big = Msg::Send {
            dst: DEV,
            data: &zeros,
        };
        let w = wire(&big);
        assert!(w.len() <= MAX_WIRE);
        let mut scratch = [0u8; MAX_WIRE];
        assert_eq!(Msg::from_run(&w[..w.len() - 1], &mut scratch), Ok(big));
    }

    #[test]
    fn o5_golden_frames_match_the_bridge_firmware() {
        for (msg, want) in golden() {
            assert_eq!(hex(&wire(&msg)), want, "{msg:?}");
        }
    }

    #[test]
    fn eg1_encode_into_exact_buffer() {
        for (msg, want) in golden() {
            let len = want.len() / 2;
            let mut out = vec![0u8; len];
            assert_eq!(msg.encode_wire(&mut out), Ok(len), "{msg:?}");
            assert_eq!(hex(&out), want);
            let r = raw(&msg).unwrap();
            let mut exact = vec![0u8; r.len()];
            assert_eq!(msg.encode_raw(&mut exact), Ok(r.len()));
            assert_eq!(exact, r);
        }
    }

    #[test]
    fn eg1_small_output_buffer_is_an_error() {
        for (msg, want) in golden() {
            let len = want.len() / 2;
            let mut out = vec![0u8; len - 1];
            assert_eq!(msg.encode_wire(&mut out), Err(BridgeError::BufferTooSmall));
            let r = raw(&msg).unwrap();
            let mut short = vec![0u8; r.len() - 1];
            assert_eq!(msg.encode_raw(&mut short), Err(BridgeError::BufferTooSmall));
        }
        assert_eq!(
            Msg::Query.encode_wire(&mut []),
            Err(BridgeError::BufferTooSmall)
        );
    }

    #[test]
    fn o5_bad_frames_are_rejected() {
        assert_eq!(
            raw(&Msg::Send {
                dst: DEV,
                data: &[]
            }),
            Err(BridgeError::Malformed)
        );
        assert_eq!(
            raw(&Msg::Recv {
                src: DEV,
                data: &[0; MAX_DATA + 1]
            }),
            Err(BridgeError::Malformed)
        );
        let mut r = raw(&Msg::Query).unwrap();
        r[0] ^= 1;
        assert_eq!(Msg::from_raw(&r), Err(BridgeError::Crc));
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
        let mut scratch = [0u8; MAX_WIRE];
        assert_eq!(
            Msg::from_run(&[0x05, 1, 0], &mut scratch),
            Err(BridgeError::Cobs)
        );
        let long = [1u8; MAX_WIRE + 1];
        assert_eq!(Msg::from_run(&long, &mut scratch), Err(BridgeError::Cobs));
    }

    #[test]
    fn eg2_frames_split_out_of_a_noisy_stream() {
        let mut stream = b"\r\n ets Jan  8 2013,rst cause:2\r\n".to_vec();
        stream.extend(wire(&Msg::Hello {
            mac: BRIDGE,
            channel: 1,
            version: 1,
        }));
        stream.extend(wire(&Msg::Sent { ok: true, dst: DEV }));
        stream.extend(b"garbage\x00");
        stream.extend(wire(&Msg::Recv {
            src: DEV,
            data: RECV_DATA,
        }));
        let mut s: Splitter<MAX_WIRE> = Splitter::new();
        let mut got = Vec::new();
        for b in stream {
            if let Split::Frame(_) = s.push(b) {
                let mut scratch = [0u8; MAX_WIRE];
                got.push(
                    decode_run(s.frame(), &mut scratch).map(|(skip, m)| (skip, format!("{m:?}"))),
                );
            }
        }
        // The banner has no delimiter, so it shares a run with the first
        // frame; the frame is still found behind it. The garbage run is
        // reported as undecodable.
        let fmt = |m: Msg<'_>| format!("{m:?}");
        assert_eq!(
            got,
            [
                Some((
                    32,
                    fmt(Msg::Hello {
                        mac: BRIDGE,
                        channel: 1,
                        version: 1
                    })
                )),
                Some((0, fmt(Msg::Sent { ok: true, dst: DEV }))),
                None,
                Some((
                    0,
                    fmt(Msg::Recv {
                        src: DEV,
                        data: RECV_DATA
                    })
                )),
            ]
        );
        let mut scratch = [0u8; MAX_WIRE];
        assert!(decode_run(b"noise", &mut scratch).is_none());
    }
}
