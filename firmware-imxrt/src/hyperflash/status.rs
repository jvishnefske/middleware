//! The S26KS512S status register and the bounded wait for it.
//!
//! The status register (read with 0x70, see
//! [`READ_STATUS`](super::s26ks512s::READ_STATUS)) is one 16-bit word,
//! high byte first on the bus, so the two bytes FlexSPI receives are the
//! word in big-endian order ([`Status::from_rx_bytes`]). The bits used:
//!
//! | bit | name | meaning |
//! |---|---|---|
//! | 7 | DRB | device ready (1) or busy (0) |
//! | 5 | ESB | erase failed |
//! | 4 | PSB | program failed |
//! | 3 | WBASB | write-buffer program aborted |
//! | 1 | SLSB | the operation hit a protected (locked) sector |
//!
//! The error bits are only valid once DRB is set, so a busy status is
//! [`Readiness::Busy`] whatever they hold. These are the bits NXP's SDK
//! HyperFlash example tests (`0x8000` ready, `0x3200` error, in its
//! byte-swapped view), plus WBASB.
//!
//! [`wait_step`] is the pure decision of one poll in a bounded wait: the
//! driver reads the status, asks `wait_step`, and stops at anything but
//! [`WaitOutcome::Poll`].

use core::num::NonZeroU32;

const DRB: u16 = 1 << 7;
const ESB: u16 = 1 << 5;
const PSB: u16 = 1 << 4;
const WBASB: u16 = 1 << 3;
const SLSB: u16 = 1 << 1;

/// A status register value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(u16);

/// Whether the flash is still working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// An embedded operation is running.
    Busy,
    /// Idle; the last operation succeeded.
    Ready,
}

/// Why the last program or erase failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusError {
    /// ESB: the erase failed.
    EraseFailed,
    /// PSB: the program failed.
    ProgramFailed,
    /// WBASB: a write-buffer program aborted.
    WriteBufferAbort,
    /// SLSB: the target sector is protected.
    SectorLocked,
}

impl Status {
    /// From the register word.
    pub const fn from_word(word: u16) -> Status {
        Status(word)
    }

    /// From the two bytes FlexSPI received (high byte first).
    pub const fn from_rx_bytes(bytes: [u8; 2]) -> Status {
        Status(u16::from_be_bytes(bytes))
    }

    /// The register word.
    pub const fn word(self) -> u16 {
        self.0
    }

    /// Ready, busy, or the error of the finished operation.
    pub const fn check(self) -> Result<Readiness, StatusError> {
        if self.0 & DRB == 0 {
            return Ok(Readiness::Busy);
        }
        if self.0 & SLSB != 0 {
            Err(StatusError::SectorLocked)
        } else if self.0 & ESB != 0 {
            Err(StatusError::EraseFailed)
        } else if self.0 & PSB != 0 {
            Err(StatusError::ProgramFailed)
        } else if self.0 & WBASB != 0 {
            Err(StatusError::WriteBufferAbort)
        } else {
            Ok(Readiness::Ready)
        }
    }
}

/// The bound of one wait: at most `max_polls` status reads, `interval_us`
/// apart. The budgets below are conservative ceilings, well above the
/// S26KS512S's typical times (512-byte program well under 1 ms, sector
/// erase about 1 s); they guarantee the wait ends, they are not a
/// performance figure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollBound {
    max_polls: NonZeroU32,
    interval_us: u32,
}

impl PollBound {
    /// A bound of `max_polls` reads, `interval_us` apart.
    pub const fn new(max_polls: NonZeroU32, interval_us: u32) -> PollBound {
        PollBound {
            max_polls,
            interval_us,
        }
    }

    /// The most status reads.
    pub const fn max_polls(self) -> u32 {
        self.max_polls.get()
    }

    /// The pause between reads, in microseconds.
    pub const fn interval_us(self) -> u32 {
        self.interval_us
    }

    /// The whole budget in microseconds (saturating).
    pub const fn budget_us(self) -> u64 {
        self.max_polls.get() as u64 * self.interval_us as u64
    }
}

const fn nz(n: u32) -> NonZeroU32 {
    match NonZeroU32::new(n) {
        Some(n) => n,
        None => panic!("zero poll bound"),
    }
}

/// Wait after a program of up to 512 bytes: 1000 polls 10 us apart (10 ms).
pub const PROGRAM_WAIT: PollBound = PollBound::new(nz(1_000), 10);
/// Wait after a sector erase: 10 000 polls 1 ms apart (10 s).
pub const ERASE_WAIT: PollBound = PollBound::new(nz(10_000), 1_000);

/// The decision after one status read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The operation finished and succeeded.
    Done,
    /// Still busy: pause `interval_us` and read again.
    Poll,
    /// The operation finished and failed.
    Failed(StatusError),
    /// Still busy after the last allowed read.
    TimedOut,
}

/// The decision after status read number `poll` (0-based) of a wait
/// bounded by `bound`.
pub const fn wait_step(bound: PollBound, poll: u32, status: Status) -> WaitOutcome {
    match status.check() {
        Ok(Readiness::Ready) => WaitOutcome::Done,
        Err(e) => WaitOutcome::Failed(e),
        Ok(Readiness::Busy) => {
            if poll.saturating_add(1) >= bound.max_polls() {
                WaitOutcome::TimedOut
            } else {
                WaitOutcome::Poll
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt4_status_decodes_ready_and_errors() {
        assert_eq!(Status::from_word(0x0000).check(), Ok(Readiness::Busy));
        assert_eq!(Status::from_word(0x0080).check(), Ok(Readiness::Ready));
        assert_eq!(
            Status::from_word(0x00A0).check(),
            Err(StatusError::EraseFailed)
        );
        assert_eq!(
            Status::from_word(0x0090).check(),
            Err(StatusError::ProgramFailed)
        );
        assert_eq!(
            Status::from_word(0x0088).check(),
            Err(StatusError::WriteBufferAbort)
        );
        assert_eq!(
            Status::from_word(0x0082).check(),
            Err(StatusError::SectorLocked)
        );
        // Error bits while busy are not yet valid.
        assert_eq!(Status::from_word(0x0030).check(), Ok(Readiness::Busy));
    }

    #[test]
    fn rt4_status_bytes_are_high_byte_first() {
        // The SDK reads 0x8000 as ready in its little-endian u32 view:
        // bytes [0x00, 0x80].
        assert_eq!(
            Status::from_rx_bytes([0x00, 0x80]).check(),
            Ok(Readiness::Ready)
        );
        assert_eq!(
            Status::from_rx_bytes([0x80, 0x00]).check(),
            Ok(Readiness::Busy)
        );
    }

    #[test]
    fn rt4_status_wait_is_bounded() {
        let bound = PollBound::new(nz(3), 10);
        let busy = Status::from_word(0);
        assert_eq!(wait_step(bound, 0, busy), WaitOutcome::Poll);
        assert_eq!(wait_step(bound, 1, busy), WaitOutcome::Poll);
        assert_eq!(wait_step(bound, 2, busy), WaitOutcome::TimedOut);
        assert_eq!(wait_step(bound, u32::MAX, busy), WaitOutcome::TimedOut);
        assert_eq!(
            wait_step(bound, 2, Status::from_word(0x80)),
            WaitOutcome::Done
        );
        assert_eq!(
            wait_step(bound, 0, Status::from_word(0xA0)),
            WaitOutcome::Failed(StatusError::EraseFailed)
        );
        // Simulate a driver loop: it ends within max_polls reads.
        let mut reads = 0;
        let mut poll = 0;
        loop {
            reads += 1;
            match wait_step(ERASE_WAIT, poll, busy) {
                WaitOutcome::Poll => poll += 1,
                _ => break,
            }
        }
        assert_eq!(reads, ERASE_WAIT.max_polls());
        assert_eq!(PROGRAM_WAIT.budget_us(), 10_000);
        assert_eq!(ERASE_WAIT.budget_us(), 10_000_000);
    }
}
