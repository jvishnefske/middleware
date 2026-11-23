//! The A/B boot state, shared by Linux (posix-node) and U-Boot (`boot.scr`).
//!
//! Two copies, each one 512-byte sector in the unpartitioned gap before the
//! first partition ([`COPY_LBA`], 1 MiB apart so one torn write cannot hit
//! both). A copy is:
//!
//! ```text
//! bytes 0..4    CRC-32 (IEEE, as U-Boot's `crc32`), big-endian, of bytes 4..512
//! bytes 4..     text, as U-Boot's `env export -t` writes it, NUL-padded:
//!                 ab_active=a
//!                 ab_magic=emco-ab-1
//!                 ab_seq=1f
//!                 ab_tries=0
//!                 ab_try=-
//! ```
//!
//! This is the format of `kd240/fleet/proto/src/bootstate.rs` and of the
//! `boot.scr` in `kd240/nix/ab.nix`; the two are tested against the same
//! vectors. Numbers are lowercase hex without `0x`. The newest valid copy
//! (highest `ab_seq`) wins. A writer always overwrites the *other* copy with
//! `seq + 1`, so a write cut short by a power loss leaves the previous state
//! readable.
//!
//! * `ab_active`: the committed slot, booted when there is nothing to try.
//! * `ab_try`, `ab_tries`: a slot to boot `tries` more times. U-Boot
//!   decrements before it boots it; Linux commits it (`active = try`) once
//!   the system is healthy. If it never gets there, the count runs out and
//!   U-Boot boots `active` again: the rollback.
//!
//! There is no anti-rollback floor in this sector: `boot.scr` writes only
//! its own five keys, so any other key would be dropped on the first try.
//! The floor lives in posix-node's record instead.

use core::fmt;

pub const MAGIC: &str = "emco-ab-1";
pub const SECTOR: usize = 512;
/// The two copies' sectors: 2 MiB and 3 MiB into the card, in the gap
/// before partition 1 (which starts at 8 MiB).
pub const COPY_LBA: [u64; 2] = [4096, 6144];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
    pub fn parse(s: &str) -> Option<Slot> {
        match s {
            "a" => Some(Slot::A),
            "b" => Some(Slot::B),
            _ => None,
        }
    }
    /// The index of the slot in arrays (`A` 0, `B` 1), as `sys/os/info` reports it.
    pub const fn index(self) -> usize {
        match self {
            Slot::A => 0,
            Slot::B => 1,
        }
    }
    /// The slot an index names (0 A, 1 B).
    pub const fn from_index(i: u8) -> Option<Slot> {
        match i {
            0 => Some(Slot::A),
            1 => Some(Slot::B),
            _ => None,
        }
    }
    /// The slot this boot came from: `emco.slot=a|b` on the kernel command
    /// line, as boot.scr sets it.
    pub fn from_cmdline(cmdline: &str) -> Option<Slot> {
        cmdline
            .split_whitespace()
            .filter_map(|w| w.strip_prefix("emco.slot="))
            .next_back()
            .and_then(Slot::parse)
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootState {
    pub seq: u32,
    pub active: Slot,
    pub try_slot: Option<Slot>,
    pub tries: u32,
}

impl BootState {
    /// What a fresh card holds: slot A, nothing to try.
    pub fn initial() -> BootState {
        BootState {
            seq: 1,
            active: Slot::A,
            try_slot: None,
            tries: 0,
        }
    }

    /// Is there a trial boot still to come?
    pub fn pending_try(&self) -> Option<Slot> {
        self.try_slot.filter(|_| self.tries > 0)
    }

    /// The state after a trial of `slot` (one boot), from `self`: the next
    /// sequence number is the caller's ([`plan_write`]).
    pub fn with_trial(self, slot: Slot) -> BootState {
        BootState {
            try_slot: Some(slot),
            tries: 1,
            ..self
        }
    }

    /// The state once the running trial is committed: `try` is the active
    /// slot and nothing is left to try.
    pub fn committed(self) -> BootState {
        BootState {
            active: self.try_slot.unwrap_or(self.active),
            try_slot: None,
            tries: 0,
            ..self
        }
    }
}

/// CRC-32/IEEE (zlib's, U-Boot's).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// A writer over a fixed buffer, for the text (no allocation).
struct Text<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl fmt::Write for Text<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.at + s.len();
        self.buf.get_mut(self.at..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
        self.at = end;
        Ok(())
    }
}

pub fn encode(s: &BootState) -> [u8; SECTOR] {
    let mut out = [0u8; SECTOR];
    {
        // Sorted by name, as `env export` writes them.
        let mut w = Text { buf: &mut out[4..], at: 0 };
        let try_text = s.try_slot.map(Slot::as_str).unwrap_or("-");
        let written = fmt::Write::write_fmt(
            &mut w,
            format_args!(
                "ab_active={}\nab_magic={MAGIC}\nab_seq={:x}\nab_tries={:x}\nab_try={}\n",
                s.active, s.seq, s.tries, try_text
            ),
        );
        assert!(written.is_ok(), "a boot state fits its sector");
    }
    let crc = crc32(&out[4..]);
    out[..4].copy_from_slice(&crc.to_be_bytes());
    out
}

fn hex_u32(v: &str) -> Option<u32> {
    let v = v.strip_prefix("0x").unwrap_or(v);
    if v.is_empty() || v.len() > 8 {
        return None;
    }
    u32::from_str_radix(v, 16).ok()
}

/// Why a copy is not a boot state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    Length,
    Crc,
    Trailing,
    NotText,
    Line,
    Key(&'static str),
    Magic,
    Field(&'static str),
}

pub fn decode(sector: &[u8]) -> Result<BootState, Refused> {
    if sector.len() != SECTOR {
        return Err(Refused::Length);
    }
    let want = u32::from_be_bytes([sector[0], sector[1], sector[2], sector[3]]);
    if crc32(&sector[4..]) != want {
        return Err(Refused::Crc);
    }
    let body = &sector[4..];
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    if body[end..].iter().any(|&b| b != 0) {
        return Err(Refused::Trailing);
    }
    let text = core::str::from_utf8(&body[..end]).map_err(|_| Refused::NotText)?;
    let (mut magic, mut seq, mut active, mut try_slot, mut tries) = (None, None, None, None, None);
    for line in text.lines().filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once('=').ok_or(Refused::Line)?;
        let slot = match k {
            "ab_magic" => &mut magic,
            "ab_seq" => &mut seq,
            "ab_active" => &mut active,
            "ab_try" => &mut try_slot,
            "ab_tries" => &mut tries,
            _ => return Err(Refused::Key("unknown")),
        };
        if slot.replace(v).is_some() {
            return Err(Refused::Key("repeated"));
        }
    }
    if magic != Some(MAGIC) {
        return Err(Refused::Magic);
    }
    let seq = seq.and_then(hex_u32).ok_or(Refused::Field("ab_seq"))?;
    let active = active.and_then(Slot::parse).ok_or(Refused::Field("ab_active"))?;
    let try_slot = match try_slot.ok_or(Refused::Field("ab_try"))? {
        "-" => None,
        s => Some(Slot::parse(s).ok_or(Refused::Field("ab_try"))?),
    };
    let tries = tries.and_then(hex_u32).ok_or(Refused::Field("ab_tries"))?;
    Ok(BootState {
        seq,
        active,
        try_slot,
        tries,
    })
}

/// The two copies as read: which one is newest, and its state. `None` when
/// neither is valid.
pub fn newest(copies: [&[u8]; 2]) -> Option<(usize, BootState)> {
    let a = decode(copies[0]).ok();
    let b = decode(copies[1]).ok();
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.seq > a.seq { (1, b) } else { (0, a) }),
        (Some(a), None) => Some((0, a)),
        (None, Some(b)) => Some((1, b)),
        (None, None) => None,
    }
}

/// Where and what to write to replace the current state with `next`
/// (whose `seq` is ignored): the copy that is not the newest, with the next
/// sequence number.
pub fn plan_write(copies: [&[u8]; 2], next: BootState) -> (usize, [u8; SECTOR]) {
    let (index, seq) = match newest(copies) {
        Some((i, s)) => (1 - i, s.seq.wrapping_add(1)),
        None => (0, 1),
    };
    let next = BootState { seq, ..next };
    (index, encode(&next))
}

/// What U-Boot's `boot.scr` does with the state, modelled for host tests:
/// the slot it boots, and the state it writes first (a trial is recorded
/// before it boots: `tries` counts down, `try` is booted, and a hang means
/// the next reset finds `tries == 0` and boots `active`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boot {
    pub slot: Slot,
    /// The state boot.scr writes before booting, if any.
    pub wrote: Option<BootState>,
}

/// `boot.scr`'s decision from the two copies. With no valid copy it boots
/// slot A and writes nothing.
pub fn choose_boot(copies: [&[u8]; 2]) -> Boot {
    let Some((_, s)) = newest(copies) else {
        return Boot { slot: Slot::A, wrote: None };
    };
    match s.pending_try() {
        Some(try_slot) => {
            let next = BootState {
                tries: s.tries - 1,
                ..s
            };
            Boot { slot: try_slot, wrote: Some(next) }
        }
        None => Boot { slot: s.active, wrote: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(seq: u32, active: Slot, try_slot: Option<Slot>, tries: u32) -> BootState {
        BootState {
            seq,
            active,
            try_slot,
            tries,
        }
    }

    #[test]
    fn crc_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn round_trip() {
        for s in [
            BootState::initial(),
            st(0x1f, Slot::B, Some(Slot::A), 1),
            st(u32::MAX, Slot::A, None, 0),
        ] {
            assert_eq!(decode(&encode(&s)), Ok(s));
        }
    }

    #[test]
    fn the_layout_u_boot_sees() {
        let e = encode(&st(0x1f, Slot::A, Some(Slot::B), 1));
        let text = core::str::from_utf8(&e[4..]).unwrap().trim_end_matches('\0');
        assert_eq!(
            text,
            "ab_active=a\nab_magic=emco-ab-1\nab_seq=1f\nab_tries=1\nab_try=b\n"
        );
        assert_eq!(&e[..4], &crc32(&e[4..]).to_be_bytes());
    }

    #[test]
    fn the_initial_card_matches_the_lathe_image() {
        // kd240/nix/ab.nix writes exactly this text as copy 0 of a fresh card.
        let e = encode(&BootState::initial());
        let text = core::str::from_utf8(&e[4..]).unwrap().trim_end_matches('\0');
        assert_eq!(text, "ab_active=a\nab_magic=emco-ab-1\nab_seq=1\nab_tries=0\nab_try=-\n");
    }

    #[test]
    fn what_u_boot_writes_reads_back() {
        // `env export -t` output, in any order, with 0x numbers too.
        let mut s = [0u8; SECTOR];
        let text = b"ab_try=b\nab_tries=0\nab_seq=0x20\nab_magic=emco-ab-1\nab_active=a\n";
        s[4..4 + text.len()].copy_from_slice(text);
        let crc = crc32(&s[4..]);
        s[..4].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(decode(&s), Ok(st(0x20, Slot::A, Some(Slot::B), 0)));
    }

    #[test]
    fn corrupt_copies_are_refused() {
        let good = encode(&BootState::initial());
        let mut flipped = good;
        flipped[10] ^= 1;
        assert_eq!(decode(&flipped), Err(Refused::Crc));
        assert!(decode(&[0u8; SECTOR]).is_err(), "all zero: CRC of zeros is not 0");
        assert!(decode(&[0xffu8; SECTOR]).is_err());
        assert!(decode(&good[..100]).is_err());

        let with = |text: &[u8]| {
            let mut s = [0u8; SECTOR];
            s[4..4 + text.len()].copy_from_slice(text);
            let crc = crc32(&s[4..]);
            s[..4].copy_from_slice(&crc.to_be_bytes());
            s
        };
        for bad in [
            &b"ab_active=a\nab_magic=other\nab_seq=1\nab_tries=0\nab_try=-\n"[..],
            b"ab_active=c\nab_magic=emco-ab-1\nab_seq=1\nab_tries=0\nab_try=-\n",
            b"ab_active=a\nab_magic=emco-ab-1\nab_seq=1\nab_tries=0\n",
            b"ab_active=a\nab_magic=emco-ab-1\nab_seq=1\nab_tries=0\nab_try=-\nbootcmd=reset\n",
            b"ab_active=a\nab_active=b\nab_magic=emco-ab-1\nab_seq=1\nab_tries=0\nab_try=-\n",
            b"ab_active=a\nab_magic=emco-ab-1\nab_seq=zz\nab_tries=0\nab_try=-\n",
            b"ab_active=a\nab_magic=emco-ab-1\nab_seq=1\nab_tries=0\nab_try=-\n\0junk",
        ] {
            assert!(decode(&with(bad)).is_err(), "{}", core::str::from_utf8(bad).unwrap());
        }
    }

    #[test]
    fn newest_valid_copy_wins() {
        let old = encode(&st(5, Slot::A, None, 0));
        let new = encode(&st(6, Slot::A, Some(Slot::B), 1));
        assert_eq!(newest([&old, &new]), Some((1, st(6, Slot::A, Some(Slot::B), 1))));
        assert_eq!(newest([&new, &old]).unwrap().0, 0);
        let torn = [0x55u8; SECTOR];
        assert_eq!(newest([&torn, &old]), Some((1, st(5, Slot::A, None, 0))));
        assert_eq!(newest([&old, &torn]).unwrap().0, 0);
        assert_eq!(newest([&torn, &torn]), None);
    }

    #[test]
    fn writes_go_to_the_other_copy() {
        let a = encode(&st(5, Slot::A, None, 0));
        let b = encode(&st(4, Slot::A, None, 0));
        let next = st(0, Slot::A, Some(Slot::B), 1);
        let (i, bytes) = plan_write([&a, &b], next);
        assert_eq!(i, 1);
        assert_eq!(decode(&bytes), Ok(st(6, Slot::A, Some(Slot::B), 1)));
        // With nothing valid, start at copy 0, seq 1.
        let z = [0u8; SECTOR];
        let (i, bytes) = plan_write([&z, &z], next);
        assert_eq!((i, decode(&bytes).unwrap().seq), (0, 1));
    }

    #[test]
    fn a_torn_write_keeps_the_previous_state() {
        // The write of state 6 into copy 1 cut off at every byte: whatever
        // lands, the reader sees either 5 (old) or 6 (new), never a mixture.
        let a = encode(&st(5, Slot::A, None, 0));
        let b = encode(&st(4, Slot::A, None, 0));
        let (i, new) = plan_write([&a, &b], st(0, Slot::A, Some(Slot::B), 1));
        assert_eq!(i, 1);
        for cut in 0..=SECTOR {
            let mut torn = b;
            torn[..cut].copy_from_slice(&new[..cut]);
            let (_, s) = newest([&a, &torn]).expect("always something valid");
            assert!(
                s == st(5, Slot::A, None, 0) || s == st(6, Slot::A, Some(Slot::B), 1),
                "cut {cut}: {s:?}"
            );
        }
    }

    #[test]
    fn slots_and_the_command_line() {
        assert_eq!(Slot::from_cmdline("init=/x emco.slot=b quiet"), Some(Slot::B));
        assert_eq!(Slot::from_cmdline("emco.slot=a emco.slot=b"), Some(Slot::B));
        assert_eq!(Slot::from_cmdline("emco.slot=c"), None);
        assert_eq!(Slot::from_cmdline("root=/dev/sda2"), None);
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::from_index(1), Some(Slot::B));
        assert_eq!(Slot::from_index(2), None);
    }

    #[test]
    fn a_trial_is_booted_once_then_rolled_back() {
        // Committed A. Linux records a trial of B: boot.scr counts it down
        // and boots B.
        let a = encode(&st(5, Slot::A, None, 0));
        let trial = encode(&st(6, Slot::A, Some(Slot::B), 1));
        let boot = choose_boot([&a, &trial]);
        assert_eq!(boot.slot, Slot::B);
        let wrote = boot.wrote.expect("the count goes down before the trial boots");
        assert_eq!((wrote.tries, wrote.try_slot), (0, Some(Slot::B)));
        // Next reset, B never confirmed (tries 0): A boots again.
        // The write goes to the copy that is not newest (copy 0), seq 7.
        let (i, w) = plan_write([&a, &trial], wrote);
        assert_eq!(i, 0);
        let copies = [w, trial];
        assert_eq!(choose_boot([&copies[0], &copies[1]]).slot, Slot::A);
        assert!(choose_boot([&copies[0], &copies[1]]).wrote.is_none());
    }

    #[test]
    fn a_confirmed_trial_becomes_the_active_slot() {
        let trial = st(6, Slot::A, Some(Slot::B), 0);
        let c = trial.committed();
        assert_eq!((c.active, c.try_slot, c.tries), (Slot::B, None, 0));
        assert_eq!(BootState::initial().with_trial(Slot::B).tries, 1);
        // No valid copy: slot A, nothing written.
        let z = [0u8; SECTOR];
        assert_eq!(choose_boot([&z, &z]), Boot { slot: Slot::A, wrote: None });
    }
}
