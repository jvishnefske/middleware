//! The board configuration record: the settings the board keeps across
//! reboots that are not trust or firmware state. Today it holds one, the
//! Zenoh endpoint (`sys/zenoh/endpoint`): the router or peer listener the
//! board's Zenoh client connects to (Zenoh UDP scouting is not
//! implemented, so the endpoint must be configured).
//!
//! # Endpoint
//!
//! Only `tcp/<ipv4>:<port>` is accepted ([`Endpoint::parse`]): the dotted
//! quad of four decimal octets (0–255, no leading zeros, no sign) and a
//! decimal port 1–65535 (no leading zeros). Host names, IPv6, UDP, TLS and
//! query parameters are refused: the board resolves nothing and speaks
//! only Zenoh over TCP. The canonical form is what [`Endpoint`]'s
//! `Display` writes, so a parsed endpoint prints back to its own string.
//!
//! # Format and placement
//!
//! [`RECORD_LEN`] bytes, little-endian, at the start of one of the two
//! blocks of the configuration region (`crate::layout::FlashMap::config`):
//!
//! | offset | field |
//! |--------|-------|
//! | 0      | magic `MWZC` |
//! | 4      | format version ([`VERSION`], u16) |
//! | 6      | endpoint length (u8, 1..=[`MAX_ENDPOINT`]) |
//! | 7      | zero |
//! | 8      | sequence number (u32) |
//! | 12     | CRC-32 of bytes 0..12 and 16..80 |
//! | 16     | the endpoint string, zero padded to [`MAX_ENDPOINT`] bytes |
//!
//! [`save`] writes the next record (sequence number + 1) into the block
//! that does *not* hold the current one, as `crate::ab::record` does with
//! the update record: erase that block, program the endpoint (bytes
//! 16..80), then program the header with its CRC (bytes 0..16). A power
//! cut before the header is complete leaves a block without the magic or
//! with a CRC that does not match, and the other block, never touched by
//! the save, still holds the previous record: [`load`] decodes both blocks
//! and takes the valid one with the higher sequence number, so every save
//! is atomic. A board with no valid record has no endpoint.

use core::fmt;
use core::str;

use crate::ab::io::{IoError, read_at, write_at};
use crate::layout::{BLOCK, Region};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use middleware::Key;
use middleware::crc::Crc32;

/// The request key of the Zenoh endpoint (`crate::service`): an empty body
/// reads it, a body sets it. Never accepted over Zenoh
/// (`crate::zenoh_mgmt`).
pub const ZENOH_ENDPOINT: Key<'static> = Key::from_static("sys/zenoh/endpoint");

/// Encoded record length.
pub const RECORD_LEN: usize = 80;
/// Longest endpoint string the record holds.
pub const MAX_ENDPOINT: usize = 64;
/// Record magic.
pub const MAGIC: [u8; 4] = *b"MWZC";
/// Record format version.
pub const VERSION: u16 = 1;
/// Where the endpoint string starts in the record (the header's length).
const BODY: usize = 16;

/// A Zenoh endpoint the board connects to: `tcp/<addr>:<port>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// IPv4 address, most significant octet first.
    pub addr: [u8; 4],
    /// TCP port (never 0).
    pub port: u16,
}

/// Why an endpoint string was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointError {
    /// Not UTF-8.
    Utf8,
    /// Longer than [`MAX_ENDPOINT`] bytes.
    TooLong,
    /// Not `tcp/` (another protocol, or no protocol).
    Protocol,
    /// The address is not four decimal octets 0–255 without leading zeros.
    Address,
    /// The port is missing, not decimal, has a leading zero, or is outside
    /// 1–65535.
    Port,
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EndpointError::Utf8 => "endpoint is not UTF-8",
            EndpointError::TooLong => "endpoint longer than 64 bytes",
            EndpointError::Protocol => "endpoint must start with tcp/",
            EndpointError::Address => "endpoint address must be an IPv4 dotted quad",
            EndpointError::Port => "endpoint port must be 1-65535",
        })
    }
}

/// A decimal number of 1..=`max_digits` digits, no leading zero (except
/// `0` itself), at most `max`.
fn decimal(s: &str, max_digits: usize, max: u32) -> Option<u32> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > max_digits || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if b.len() > 1 && b[0] == b'0' {
        return None;
    }
    // At most five digits: no overflow.
    let v = b.iter().fold(0u32, |n, d| n * 10 + u32::from(d - b'0'));
    (v <= max).then_some(v)
}

impl Endpoint {
    /// Parses `tcp/<a>.<b>.<c>.<d>:<port>`. See the module docs.
    pub fn parse(s: &str) -> Result<Endpoint, EndpointError> {
        if s.len() > MAX_ENDPOINT {
            return Err(EndpointError::TooLong);
        }
        let rest = s.strip_prefix("tcp/").ok_or(EndpointError::Protocol)?;
        let (host, port) = rest.rsplit_once(':').ok_or(EndpointError::Port)?;
        let mut addr = [0u8; 4];
        let mut parts = host.split('.');
        for octet in &mut addr {
            let p = parts.next().ok_or(EndpointError::Address)?;
            let v = decimal(p, 3, 255).ok_or(EndpointError::Address)?;
            *octet = u8::try_from(v).map_err(|_| EndpointError::Address)?;
        }
        if parts.next().is_some() {
            return Err(EndpointError::Address);
        }
        let port = decimal(port, 5, u32::from(u16::MAX))
            .and_then(|p| u16::try_from(p).ok())
            .filter(|&p| p != 0)
            .ok_or(EndpointError::Port)?;
        Ok(Endpoint { addr, port })
    }

    /// Parses a request body: UTF-8, then [`Endpoint::parse`].
    pub fn parse_bytes(b: &[u8]) -> Result<Endpoint, EndpointError> {
        if b.len() > MAX_ENDPOINT {
            return Err(EndpointError::TooLong);
        }
        Endpoint::parse(str::from_utf8(b).map_err(|_| EndpointError::Utf8)?)
    }

    /// Writes the canonical string into `out`; its length, or `None` when
    /// `out` is too small (the string is at most 25 bytes).
    pub fn write_to(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = SliceWriter { buf: out, len: 0 };
        fmt::write(&mut w, format_args!("{self}")).ok()?;
        Some(w.len)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.addr;
        write!(f, "tcp/{a}.{b}.{c}.{d}:{}", self.port)
    }
}

/// `fmt::Write` into a byte slice, failing when it is full.
struct SliceWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl fmt::Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        self.buf
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// The board configuration. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The Zenoh endpoint (`sys/zenoh/endpoint`).
    pub endpoint: Endpoint,
}

/// The CRC of a record: header bytes 0..12 and the body.
fn record_crc(b: &[u8; RECORD_LEN]) -> u32 {
    Crc32::new().update(&b[..12]).update(&b[BODY..]).finish()
}

impl Config {
    /// Encodes with sequence number `seq`.
    pub fn encode(&self, seq: u32) -> [u8; RECORD_LEN] {
        let mut out = [0u8; RECORD_LEN];
        // At most 25 bytes, inside the 64-byte body.
        let len = self.endpoint.write_to(&mut out[BODY..]).unwrap_or(0);
        out[..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&VERSION.to_le_bytes());
        out[6] = u8::try_from(len).unwrap_or(0);
        out[8..12].copy_from_slice(&seq.to_le_bytes());
        let crc = record_crc(&out);
        out[12..16].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decodes a record and its sequence number; `None` unless the magic,
    /// version, CRC, zero bytes and the endpoint (canonical, as
    /// [`Endpoint::parse`] accepts it) are valid.
    pub fn decode(b: &[u8]) -> Option<(u32, Config)> {
        let b: &[u8; RECORD_LEN] = b.get(..RECORD_LEN)?.try_into().ok()?;
        let crc = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
        if b[..4] != MAGIC
            || u16::from_le_bytes([b[4], b[5]]) != VERSION
            || b[7] != 0
            || record_crc(b) != crc
        {
            return None;
        }
        let len = usize::from(b[6]);
        let (text, pad) = b[BODY..].split_at_checked(len)?;
        if len == 0 || pad.iter().any(|&x| x != 0) {
            return None;
        }
        let endpoint = Endpoint::parse_bytes(text).ok()?;
        let seq = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
        Some((seq, Config { endpoint }))
    }
}

/// The configuration as found on flash, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stored {
    /// The configuration, `None` when no record is valid.
    pub config: Option<Config>,
    /// The block (0 or 1) and sequence number of the valid record, if any.
    pub at: Option<(u32, u32)>,
}

impl Stored {
    /// No valid record on flash.
    pub const EMPTY: Stored = Stored {
        config: None,
        at: None,
    };

    /// The stored endpoint, if any.
    pub fn endpoint(&self) -> Option<Endpoint> {
        self.config.map(|c| c.endpoint)
    }
}

/// Reads both blocks of `region` (the configuration region) and returns
/// the newest valid record.
pub fn load<F: ReadNorFlash>(flash: &mut F, region: Region) -> Result<Stored, IoError<F::Error>> {
    let mut best: Option<(u32, u32, Config)> = None;
    for block in 0..2u32 {
        let mut b = [0u8; RECORD_LEN];
        read_at(flash, region.offset + block * BLOCK, &mut b)?;
        if let Some((seq, c)) = Config::decode(&b)
            && best.is_none_or(|(_, s, _)| seq > s)
        {
            best = Some((block, seq, c));
        }
    }
    Ok(match best {
        Some((block, seq, config)) => Stored {
            config: Some(config),
            at: Some((block, seq)),
        },
        None => Stored::EMPTY,
    })
}

/// Saves `next` after `current` (what [`load`] or the previous save
/// returned): erases the other block, programs the endpoint, then the
/// header with the next sequence number and the CRC. On error the flash
/// still holds `current` as the newest valid record.
pub fn save<F: NorFlash>(
    flash: &mut F,
    region: Region,
    current: &Stored,
    next: Config,
) -> Result<Stored, IoError<F::Error>> {
    let (block, seq) = match current.at {
        Some((b, s)) => (1 - b.min(1), s.wrapping_add(1)),
        None => (0, 1),
    };
    let at = region.offset + block * BLOCK;
    let rec = next.encode(seq);
    flash.erase(at, at + BLOCK).map_err(IoError::Flash)?;
    // The body first, the header (magic, sequence, CRC) last: a record is
    // valid only once both are on flash.
    write_at(flash, at + BODY as u32, &rec[BODY..])?;
    write_at(flash, at, &rec[..BODY])?;
    Ok(Stored {
        config: Some(next),
        at: Some((block, seq)),
    })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::ab::test_flash::{BlockFlash, Op};
    use crate::layout::RT1050_EVKB;
    use std::string::ToString;

    const REGION: Region = RT1050_EVKB.config;

    fn ep(s: &str) -> Endpoint {
        Endpoint::parse(s).unwrap()
    }

    fn cfg(s: &str) -> Config {
        Config { endpoint: ep(s) }
    }

    #[test]
    fn fl7_endpoints_parse_only_tcp_ipv4_and_print_back() {
        for s in [
            "tcp/192.168.1.10:7447",
            "tcp/0.0.0.0:1",
            "tcp/255.255.255.255:65535",
            "tcp/10.0.0.1:80",
        ] {
            let e = ep(s);
            assert_eq!(e.to_string(), s);
            let mut buf = [0u8; MAX_ENDPOINT];
            let n = e.write_to(&mut buf).unwrap();
            assert_eq!(&buf[..n], s.as_bytes());
            assert_eq!(Endpoint::parse_bytes(s.as_bytes()), Ok(e));
        }
        assert_eq!(
            ep("tcp/192.168.1.10:7447"),
            Endpoint {
                addr: [192, 168, 1, 10],
                port: 7447
            }
        );
        // The longest canonical endpoint fits a 25-byte buffer, not 24.
        assert_eq!(ep("tcp/255.255.255.255:65535").write_to(&mut [0; 24]), None);
    }

    #[test]
    fn fl7_everything_else_is_refused() {
        use EndpointError::*;
        let long = "tcp/1.2.3.4:".to_string() + &"1".repeat(60);
        for (s, why) in [
            ("", Protocol),
            ("192.168.1.10:7447", Protocol),
            ("udp/192.168.1.10:7447", Protocol),
            ("tls/192.168.1.10:7447", Protocol),
            ("TCP/192.168.1.10:7447", Protocol),
            (" tcp/192.168.1.10:7447", Protocol),
            ("tcp/router.local:7447", Address),
            ("tcp/[::1]:7447", Address),
            ("tcp/192.168.1:7447", Address),
            ("tcp/192.168.1.10.1:7447", Address),
            ("tcp/192.168.1.256:7447", Address),
            ("tcp/192.168.01.1:7447", Address),
            ("tcp/192.168..1:7447", Address),
            ("tcp/192.168.1.+1:7447", Address),
            ("tcp/192.168.1.1000:7447", Address),
            ("tcp/:7447", Address),
            ("tcp/192.168.1.10", Port),
            ("tcp/192.168.1.10:", Port),
            ("tcp/192.168.1.10:0", Port),
            ("tcp/192.168.1.10:65536", Port),
            ("tcp/192.168.1.10:07447", Port),
            ("tcp/192.168.1.10:+80", Port),
            ("tcp/192.168.1.10:7447 ", Port),
            ("tcp/192.168.1.10:7447#iface=eth0", Port),
            ("tcp/192.168.1.10:7447?x=1", Port),
            ("tcp/192.168.1.10:999999", Port),
            (long.as_str(), TooLong),
        ] {
            assert_eq!(Endpoint::parse(s), Err(why), "{s:?}");
        }
        assert_eq!(Endpoint::parse_bytes(&[0xff, 0xfe]), Err(Utf8));
        assert_eq!(Endpoint::parse_bytes(&[b'1'; 65]), Err(TooLong));
        for e in [Utf8, TooLong, Protocol, Address, Port] {
            assert!(!e.to_string().is_empty());
        }
    }

    #[test]
    fn fl7_records_round_trip_and_reject_damage() {
        let c = cfg("tcp/192.168.1.10:7447");
        for seq in [1, u32::MAX] {
            let bytes = c.encode(seq);
            assert_eq!(Config::decode(&bytes), Some((seq, c)));
            for bit in 0..RECORD_LEN * 8 {
                let mut torn = bytes;
                torn[bit / 8] ^= 1 << (bit % 8);
                assert_eq!(Config::decode(&torn), None, "bit {bit}");
            }
        }
        assert_eq!(Config::decode(&[0xFF; RECORD_LEN]), None);
        assert_eq!(Config::decode(&[0; RECORD_LEN]), None);
        assert_eq!(Config::decode(&c.encode(1)[..RECORD_LEN - 1]), None);
        // A record with a valid CRC but a non-canonical or foreign endpoint
        // is refused too.
        let mut foreign = c.encode(1);
        let s = b"udp/192.168.1.10:7447";
        foreign[BODY..BODY + s.len()].copy_from_slice(s);
        let crc = record_crc(&foreign);
        foreign[12..16].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Config::decode(&foreign), None);
        // Another format version is not read.
        let mut v2 = c.encode(1);
        v2[4] = 2;
        let crc = record_crc(&v2);
        v2[12..16].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Config::decode(&v2), None);
    }

    #[test]
    fn fl7_saves_alternate_blocks_and_load_takes_the_newest() {
        let mut flash = BlockFlash::erased(REGION.end() as usize);
        let s0 = load(&mut flash, REGION).unwrap();
        assert_eq!(s0, Stored::EMPTY);
        assert_eq!(s0.endpoint(), None);
        let a = cfg("tcp/10.0.0.1:7447");
        let s1 = save(&mut flash, REGION, &s0, a).unwrap();
        assert_eq!(s1.at, Some((0, 1)));
        assert_eq!(load(&mut flash, REGION).unwrap(), s1);
        let b = cfg("tcp/10.0.0.2:7447");
        let s2 = save(&mut flash, REGION, &s1, b).unwrap();
        assert_eq!(s2.at, Some((1, 2)));
        assert_eq!(load(&mut flash, REGION).unwrap(), s2);
        let c = cfg("tcp/10.0.0.3:7448");
        let s3 = save(&mut flash, REGION, &s2, c).unwrap();
        assert_eq!(s3.at, Some((0, 3)));
        assert_eq!(load(&mut flash, REGION).unwrap(), s3);
        assert_eq!(s3.endpoint(), Some(c.endpoint));
        // Each save: one whole-block erase of its own block, the body, then
        // the header.
        let at = REGION.offset;
        assert_eq!(
            flash.log[..3],
            [
                Op::Erase(at, at + BLOCK),
                Op::Write(at + BODY as u32, RECORD_LEN - BODY),
                Op::Write(at, BODY),
            ]
        );
        assert!(flash.log.iter().all(|op| match *op {
            Op::Erase(from, to) => from >= REGION.offset && to <= REGION.end(),
            Op::Write(w, n) => w >= REGION.offset && w as usize + n <= REGION.end() as usize,
        }));
    }

    #[test]
    fn fl7_a_cut_at_every_flash_op_leaves_the_old_record_or_the_new_one() {
        let mut base = BlockFlash::erased(REGION.end() as usize);
        let first = cfg("tcp/10.0.0.1:7447");
        let s0 = load(&mut base, REGION).unwrap();
        let s1 = save(&mut base, REGION, &s0, first).unwrap();
        let next = cfg("tcp/192.168.100.200:65000");
        // The first save too: from no record, a cut leaves no record.
        for (start, current, old) in [
            (BlockFlash::erased(REGION.end() as usize), s0, None),
            (base.clone(), s1, Some(first)),
        ] {
            for cut in 0..4 {
                let mut flash = start.clone();
                flash.cut_at(cut);
                let done = save(&mut flash, REGION, &current, next).is_ok();
                let ops = flash.ops();
                flash.restore();
                let got = load(&mut flash, REGION).unwrap().config;
                assert_eq!(got, if done { Some(next) } else { old }, "cut {cut}");
                assert_eq!(done, cut >= 3, "a save is an erase and two programs");
                assert!(ops <= 3);
                // After the cut the next save still works and wins.
                if !done {
                    let now = load(&mut flash, REGION).unwrap();
                    let s = save(&mut flash, REGION, &now, next).unwrap();
                    assert_eq!(load(&mut flash, REGION).unwrap(), s);
                }
            }
        }
    }

    #[test]
    fn fl7_a_cut_into_a_stale_block_never_resurrects_it() {
        // Two saves: block 0 holds seq 1 (stale), block 1 seq 2 (current).
        let mut base = BlockFlash::erased(REGION.end() as usize);
        let s1 = save(&mut base, REGION, &Stored::EMPTY, cfg("tcp/10.0.0.1:1")).unwrap();
        let s2 = save(&mut base, REGION, &s1, cfg("tcp/10.0.0.2:2")).unwrap();
        let next = cfg("tcp/10.0.0.3:3");
        for cut in 0..3 {
            let mut flash = base.clone();
            flash.cut_at(cut);
            assert!(save(&mut flash, REGION, &s2, next).is_err());
            flash.restore();
            // The torn block 0 is neither seq 1 nor seq 3: seq 2 is read.
            assert_eq!(load(&mut flash, REGION).unwrap(), s2, "cut {cut}");
        }
    }
}
