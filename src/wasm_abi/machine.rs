//! ABI 9: the messages of the machine channel kinds (MC1–MC4).
//!
//! A machine payload (a physical model: axes, a spindle, switches, a
//! turret, the e-stop) declares channels of these kinds
//! ([`super::channels::DataKind::Digital`] and after) and binds them to a
//! node's control-module points ([`crate::loader::control`]): native
//! drivers on the node (a simulator, a board's GPIO lines, LinuxCNC) read
//! and write the hardware; the payload only sees these messages. No
//! payload ever touches a register, and none generates steps: a driver
//! takes whole [`Segment`]s and runs them in its own time base (the PL, a
//! kernel driver, LinuxCNC's motion controller), so the payload's
//! scheduling never decides a step's timing.
//!
//! Every message has one fixed length, little-endian, and decodes only at
//! exactly that length (`None` otherwise), so a message of one kind can
//! never be taken for another.
//!
//! | kind | message | bytes | layout |
//! |------|---------|-------|--------|
//! | digital  | [`Digital`]    | 9  | `[on u8 (0/1)][at_us u64]` |
//! | position | [`Position`]   | 16 | `[counts i64][at_us u64]` |
//! | setpoint | [`Setpoint`]   | 4  | `[value i32]` |
//! | segments, out | [`Segment`] | 17 | `[seq u32][x i32][z i32][feed u32][flags u8]` |
//! | segments, in  | [`QueueState`] | 7 | `[free u16][done u32][flags u8]` |
//!
//! Units are the point's ([`crate::loader::control::Unit`]): positions in
//! counts with a scale the point states, segment end points in µm, feeds
//! in µm/s (or µm per spindle revolution with [`Segment::SYNC`]), speeds in
//! rpm. Time stamps are the node's monotonic clock in µs.
//!
//! # Back-pressure
//!
//! `channel_publish` only queues a message; the driver takes segments when
//! it runs. The queue says what it can take on its own input channel: a
//! [`QueueState`] after every change, with `free` slots and `done`, the
//! `seq` of the last segment finished. A payload keeps at most `free`
//! segments in flight; one sent to a full queue is dropped and counted
//! ([`QueueState::OVERRUN`]), never blocked on. A segment with
//! [`Segment::FLUSH`] empties the queue and stops motion (a feed hold).

/// A two-state signal at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digital {
    /// Asserted (a switch made, the e-stop chain closed).
    pub on: bool,
    /// When it was read, µs.
    pub at_us: u64,
}

impl Digital {
    /// Encoded length.
    pub const LEN: usize = 9;

    /// The message.
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0] = self.on as u8;
        b[1..].copy_from_slice(&self.at_us.to_le_bytes());
        b
    }

    /// From a message; `None` unless exactly [`Self::LEN`] bytes with a
    /// state of 0 or 1.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LEN] = b.try_into().ok()?;
        let on = match b[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        Some(Digital {
            on,
            at_us: u64_at(b, 1),
        })
    }
}

/// A measured position at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Counts (encoder or steps), in the point's scale.
    pub counts: i64,
    /// When it was read, µs.
    pub at_us: u64,
}

impl Position {
    /// Encoded length.
    pub const LEN: usize = 16;

    /// The message.
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..8].copy_from_slice(&self.counts.to_le_bytes());
        b[8..].copy_from_slice(&self.at_us.to_le_bytes());
        b
    }

    /// From a message; `None` unless exactly [`Self::LEN`] bytes.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LEN] = b.try_into().ok()?;
        Some(Position {
            counts: u64_at(b, 0) as i64,
            at_us: u64_at(b, 8),
        })
    }
}

/// A commanded value, in the point's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setpoint {
    /// The value (rpm, a turret station, …).
    pub value: i32,
}

impl Setpoint {
    /// Encoded length.
    pub const LEN: usize = 4;

    /// The message.
    pub fn encode(&self) -> [u8; Self::LEN] {
        self.value.to_le_bytes()
    }

    /// From a message; `None` unless exactly [`Self::LEN`] bytes.
    pub fn decode(b: &[u8]) -> Option<Self> {
        Some(Setpoint {
            value: i32::from_le_bytes(b.try_into().ok()?),
        })
    }
}

/// One straight move of the X and Z axes, to an absolute end point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Sequence number, increasing; [`QueueState::done`] echoes it.
    pub seq: u32,
    /// X end point, µm (diameter or radius is the physical model's
    /// convention; the driver moves the axis it is bound to).
    pub x_um: i32,
    /// Z end point, µm.
    pub z_um: i32,
    /// Feed: µm/s along the move, or µm per spindle revolution with
    /// [`Segment::SYNC`]; ignored with [`Segment::RAPID`].
    pub feed: u32,
    /// [`Segment::RAPID`], [`Segment::SYNC`], [`Segment::FLUSH`].
    pub flags: u8,
}

impl Segment {
    /// Encoded length.
    pub const LEN: usize = 17;
    /// At the axes' rapid rate.
    pub const RAPID: u8 = 1 << 0;
    /// Feed per spindle revolution (threading, feed per rev).
    pub const SYNC: u8 = 1 << 1;
    /// Empty the queue and stop (this segment itself is not run).
    pub const FLUSH: u8 = 1 << 7;
    /// Flags a driver knows; others are refused.
    pub const KNOWN: u8 = Self::RAPID | Self::SYNC | Self::FLUSH;

    /// The message.
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..4].copy_from_slice(&self.seq.to_le_bytes());
        b[4..8].copy_from_slice(&self.x_um.to_le_bytes());
        b[8..12].copy_from_slice(&self.z_um.to_le_bytes());
        b[12..16].copy_from_slice(&self.feed.to_le_bytes());
        b[16] = self.flags;
        b
    }

    /// From a message; `None` unless exactly [`Self::LEN`] bytes with
    /// known flags, and not both rapid and synchronised.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LEN] = b.try_into().ok()?;
        let flags = b[16];
        if flags & !Self::KNOWN != 0
            || flags & (Self::RAPID | Self::SYNC) == Self::RAPID | Self::SYNC
        {
            return None;
        }
        Some(Segment {
            seq: u32_at(b, 0),
            x_um: u32_at(b, 4) as i32,
            z_um: u32_at(b, 8) as i32,
            feed: u32_at(b, 12),
            flags,
        })
    }

    /// A feed hold: empty the queue.
    pub const fn flush(seq: u32) -> Self {
        Segment {
            seq,
            x_um: 0,
            z_um: 0,
            feed: 0,
            flags: Self::FLUSH,
        }
    }
}

/// The segment queue's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueState {
    /// Segments it can take now.
    pub free: u16,
    /// `seq` of the last segment finished (0: none yet).
    pub done: u32,
    /// [`QueueState::IDLE`], [`QueueState::STOPPED`], [`QueueState::OVERRUN`].
    pub flags: u8,
}

impl QueueState {
    /// Encoded length.
    pub const LEN: usize = 7;
    /// Nothing queued and the axes at rest.
    pub const IDLE: u8 = 1 << 0;
    /// Motion is stopped and the queue refuses segments: the e-stop, a
    /// limit switch, a driver fault. Cleared by the driver, never by the
    /// payload.
    pub const STOPPED: u8 = 1 << 1;
    /// A segment arrived with no room and was dropped (since the last
    /// state sent).
    pub const OVERRUN: u8 = 1 << 2;

    /// The message.
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..2].copy_from_slice(&self.free.to_le_bytes());
        b[2..6].copy_from_slice(&self.done.to_le_bytes());
        b[6] = self.flags;
        b
    }

    /// From a message; `None` unless exactly [`Self::LEN`] bytes.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LEN] = b.try_into().ok()?;
        Some(QueueState {
            free: u16::from_le_bytes([b[0], b[1]]),
            done: u32_at(b, 2),
            flags: b[6],
        })
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mc1_every_message_round_trips_at_its_one_length() {
        let d = Digital {
            on: true,
            at_us: 0x0102_0304_0506_0708,
        };
        assert_eq!(Digital::decode(&d.encode()), Some(d));
        let p = Position {
            counts: -123_456_789_012,
            at_us: 77,
        };
        assert_eq!(Position::decode(&p.encode()), Some(p));
        let s = Setpoint { value: -2500 };
        assert_eq!(Setpoint::decode(&s.encode()), Some(s));
        let g = Segment {
            seq: 9,
            x_um: -15_000,
            z_um: -42_500,
            feed: 1500,
            flags: Segment::SYNC,
        };
        assert_eq!(Segment::decode(&g.encode()), Some(g));
        let q = QueueState {
            free: 31,
            done: 8,
            flags: QueueState::IDLE | QueueState::OVERRUN,
        };
        assert_eq!(QueueState::decode(&q.encode()), Some(q));
        // Lengths are distinct, so no message decodes as another kind.
        let lens = [
            Digital::LEN,
            Position::LEN,
            Setpoint::LEN,
            Segment::LEN,
            QueueState::LEN,
        ];
        for (i, a) in lens.iter().enumerate() {
            for b in &lens[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn mc1_wrong_lengths_and_values_are_refused() {
        assert_eq!(Digital::decode(&[1; 8]), None);
        assert_eq!(Digital::decode(&[2, 0, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(Position::decode(&[0; 17]), None);
        assert_eq!(Setpoint::decode(&[0; 3]), None);
        let mut g = Segment::flush(3).encode();
        assert_eq!(Segment::decode(&g).map(|s| s.flags), Some(Segment::FLUSH));
        g[16] = 0x40;
        assert_eq!(Segment::decode(&g), None, "unknown flag");
        g[16] = Segment::RAPID | Segment::SYNC;
        assert_eq!(Segment::decode(&g), None, "a rapid is never synchronised");
        assert_eq!(QueueState::decode(&[0; 6]), None);
    }
}
