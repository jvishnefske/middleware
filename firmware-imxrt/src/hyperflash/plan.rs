//! Plans: the FlexSPI IP commands a driver issues for each flash operation.
//!
//! A plan is pure data. The driver executes [`Step`]s in order: an
//! [`Step::Issue`] is one IP command (IPCR0 = [`IpCommand::address`],
//! IPCR1 = the sequence run and data length), a [`Step::WaitReady`] is a
//! status poll loop bounded by its [`PollBound`] and decided by
//! [`wait_step`](super::status::wait_step).
//!
//! * A program is split by [`program_chunks`] into chunks that never cross
//!   a 512-byte write-buffer boundary; each chunk is unlock, word program
//!   with the data burst, wait ([`ProgramChunk::steps`]).
//! * An erase is one 256 KiB sector at a time: unlock, sector erase, wait
//!   ([`erase_sector_steps`]); [`erase_sectors`] lists the sectors of an
//!   aligned range.
//! * CFI: [`cfi_enter`], [`cfi_read`], [`cfi_exit`].
//!
//! Addresses are byte offsets from the start of the flash. FlexSPI runs
//! word addressed (the FCB's word-addressable option), so it halves them
//! on the bus; programs and reads must therefore start on an even offset
//! and programs move whole 16-bit words.

use super::s26ks512s::{
    CFI_ENTRY_ADDR, CMD_CFI_ENTRY, CMD_CLEAR_STATUS, CMD_RESET_ASO_EXIT, UNLOCK_ADDR_1,
};
use super::s26ks512s::{ERASE_SECTOR, PROGRAM, READ_DATA, READ_STATUS, UNLOCK, WRITE_DATA};
use super::status::{ERASE_WAIT, PROGRAM_WAIT, PollBound};
use super::{FLASH_SIZE, SECTOR_COUNT, SECTOR_SIZE, WRITE_BUFFER_SIZE};
use crate::hyperflash::lut::SequenceRun;

/// Why a plan was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The range runs past the end of the 64 MiB flash.
    OutOfRange,
    /// An odd offset or length where whole 16-bit words are needed.
    OddAlignment,
    /// An erase range that does not start and end on a sector boundary.
    UnalignedSector,
    /// A read of zero bytes or more than one IP command carries (65535).
    BadLength,
}

/// Data moved by an IP command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    /// None (a command only).
    None,
    /// Receive `len` bytes.
    Read {
        /// Bytes to receive.
        len: u16,
    },
    /// Send `len` bytes of the caller's data, starting at `start` in it.
    WriteData {
        /// Offset into the caller's buffer.
        start: usize,
        /// Bytes to send.
        len: u16,
    },
    /// Send this one word, high byte first.
    WriteWord([u8; 2]),
}

/// One FlexSPI IP command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpCommand {
    /// Byte offset in the flash (IPCR0).
    pub address: u32,
    /// The LUT sequences to run (IPCR1 ISEQID / ISEQNUM).
    pub sequence: SequenceRun,
    /// The data phase (IPCR1 IDATSZ and the FIFO direction).
    pub transfer: Transfer,
}

/// One step of a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Issue this IP command and wait for it to complete.
    Issue(IpCommand),
    /// Poll [`read_status`] until ready, failed or out of polls.
    WaitReady(PollBound),
}

/// The IP command that reads the status register (2 bytes).
pub const fn read_status() -> IpCommand {
    IpCommand {
        address: 0,
        sequence: READ_STATUS,
        transfer: Transfer::Read { len: 2 },
    }
}

const fn unlock(address: u32) -> Step {
    Step::Issue(IpCommand {
        address,
        sequence: UNLOCK,
        transfer: Transfer::None,
    })
}

const fn check_range(offset: u32, len: usize) -> Result<(), PlanError> {
    if len > FLASH_SIZE as usize || offset > FLASH_SIZE - len as u32 {
        Err(PlanError::OutOfRange)
    } else {
        Ok(())
    }
}

/// One program chunk: inside one 512-byte write buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramChunk {
    offset: u32,
    data_start: usize,
    len: u16,
}

impl ProgramChunk {
    /// Flash byte offset of the chunk.
    pub const fn offset(&self) -> u32 {
        self.offset
    }

    /// Where the chunk's bytes start in the caller's data.
    pub const fn data_start(&self) -> usize {
        self.data_start
    }

    /// Bytes in the chunk (even, 2..=512).
    pub const fn len(&self) -> u16 {
        self.len
    }

    /// Never empty (kept for the `len`/`is_empty` pair).
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Unlock, word program with the data burst, bounded wait.
    pub const fn steps(&self) -> [Step; 3] {
        [
            unlock(self.offset),
            Step::Issue(IpCommand {
                address: self.offset,
                sequence: PROGRAM,
                transfer: Transfer::WriteData {
                    start: self.data_start,
                    len: self.len,
                },
            }),
            Step::WaitReady(PROGRAM_WAIT),
        ]
    }
}

/// The chunks of a program, in address order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramChunks {
    next: u32,
    end: u32,
    data_start: usize,
}

impl Iterator for ProgramChunks {
    type Item = ProgramChunk;

    fn next(&mut self) -> Option<ProgramChunk> {
        if self.next >= self.end {
            return None;
        }
        let to_boundary = WRITE_BUFFER_SIZE - self.next % WRITE_BUFFER_SIZE;
        let len = to_boundary.min(self.end - self.next);
        let chunk = ProgramChunk {
            offset: self.next,
            data_start: self.data_start,
            len: len as u16,
        };
        self.next += len;
        self.data_start += len as usize;
        Some(chunk)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = if self.next >= self.end {
            0
        } else {
            let first = self.next / WRITE_BUFFER_SIZE;
            let last = (self.end - 1) / WRITE_BUFFER_SIZE;
            (last - first + 1) as usize
        };
        (n, Some(n))
    }
}

impl ExactSizeIterator for ProgramChunks {}

/// Plan a program of `len` bytes at `offset`: even offset and length,
/// inside the flash. The chunks never cross a write-buffer boundary.
pub fn program_chunks(offset: u32, len: usize) -> Result<ProgramChunks, PlanError> {
    check_range(offset, len)?;
    if !offset.is_multiple_of(2) || !len.is_multiple_of(2) {
        return Err(PlanError::OddAlignment);
    }
    Ok(ProgramChunks {
        next: offset,
        end: offset + len as u32,
        data_start: 0,
    })
}

/// A 256 KiB erase sector, `0..256`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sector(u16);

impl Sector {
    /// Sector `index`, or `None` past the 256 sectors.
    pub const fn new(index: u16) -> Option<Sector> {
        if (index as u32) < SECTOR_COUNT {
            Some(Sector(index))
        } else {
            None
        }
    }

    /// The sector starting at `offset`; refused unless sector aligned and
    /// inside the flash.
    pub const fn at(offset: u32) -> Result<Sector, PlanError> {
        if offset >= FLASH_SIZE {
            Err(PlanError::OutOfRange)
        } else if !offset.is_multiple_of(SECTOR_SIZE) {
            Err(PlanError::UnalignedSector)
        } else {
            Ok(Sector((offset / SECTOR_SIZE) as u16))
        }
    }

    /// The sector index.
    pub const fn index(self) -> u16 {
        self.0
    }

    /// Byte offset of the sector's start.
    pub const fn offset(self) -> u32 {
        self.0 as u32 * SECTOR_SIZE
    }
}

/// Unlock, sector erase, bounded wait: erases exactly one sector.
pub const fn erase_sector_steps(sector: Sector) -> [Step; 3] {
    let address = sector.offset();
    [
        unlock(address),
        Step::Issue(IpCommand {
            address,
            sequence: ERASE_SECTOR,
            transfer: Transfer::None,
        }),
        Step::WaitReady(ERASE_WAIT),
    ]
}

/// The sectors of an erase, in address order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sectors {
    next: u16,
    end: u16,
}

impl Iterator for Sectors {
    type Item = Sector;

    fn next(&mut self) -> Option<Sector> {
        if self.next >= self.end {
            return None;
        }
        let s = Sector(self.next);
        self.next += 1;
        Some(s)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.end.saturating_sub(self.next) as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Sectors {}

/// The sectors covering `offset..offset + len`; both ends must be sector
/// aligned and inside the flash.
pub fn erase_sectors(offset: u32, len: usize) -> Result<Sectors, PlanError> {
    check_range(offset, len)?;
    if !offset.is_multiple_of(SECTOR_SIZE) || !len.is_multiple_of(SECTOR_SIZE as usize) {
        return Err(PlanError::UnalignedSector);
    }
    let first = (offset / SECTOR_SIZE) as u16;
    let count = (len / SECTOR_SIZE as usize) as u16;
    Ok(Sectors {
        next: first,
        end: first + count,
    })
}

/// One IP read of `len` bytes at an even `offset` inside the flash.
pub const fn read(offset: u32, len: u16) -> Result<IpCommand, PlanError> {
    if len == 0 {
        return Err(PlanError::BadLength);
    }
    if let Err(e) = check_range(offset, len as usize) {
        return Err(e);
    }
    if !offset.is_multiple_of(2) {
        return Err(PlanError::OddAlignment);
    }
    Ok(IpCommand {
        address: offset,
        sequence: READ_DATA,
        transfer: Transfer::Read { len },
    })
}

/// CFI word address of the first query byte ("Q").
pub const CFI_QUERY_START_WORD: u32 = 0x10;
/// CFI words [`cfi_read`] fetches (word addresses 0x10..0x40).
pub const CFI_QUERY_WORDS: u16 = 0x30;

/// Enter ID-CFI mode: write 0x0098 to word address 0x55.
pub const fn cfi_enter() -> IpCommand {
    IpCommand {
        address: CFI_ENTRY_ADDR * 2,
        sequence: WRITE_DATA,
        transfer: Transfer::WriteWord(CMD_CFI_ENTRY.to_be_bytes()),
    }
}

/// Recovers the flash from a failed or aborted operation, before the next
/// erase or program: the Write-to-Buffer-Abort Reset (the unlock cycles,
/// then 0x00F0 to word 0x555; also an ordinary reset to read mode) and the
/// Clear Status Register command (0x0071 to word 0x555). The status
/// register's error bits (program, erase, write-buffer abort) are sticky:
/// without this, one aborted write-buffer program (a reset in the middle of
/// it) made every later operation read as failed (2026-11-14).
pub const fn recover_steps() -> [Step; 3] {
    [
        unlock(0),
        Step::Issue(IpCommand {
            address: UNLOCK_ADDR_1 * 2,
            sequence: WRITE_DATA,
            transfer: Transfer::WriteWord(CMD_RESET_ASO_EXIT.to_be_bytes()),
        }),
        Step::Issue(IpCommand {
            address: UNLOCK_ADDR_1 * 2,
            sequence: WRITE_DATA,
            transfer: Transfer::WriteWord(CMD_CLEAR_STATUS.to_be_bytes()),
        }),
    ]
}

/// Read the CFI query words 0x10..0x40 (96 bytes, see
/// [`parse_cfi`](super::cfi::parse_cfi)). Only valid after [`cfi_enter`].
pub const fn cfi_read() -> IpCommand {
    IpCommand {
        address: CFI_QUERY_START_WORD * 2,
        sequence: READ_DATA,
        transfer: Transfer::Read {
            len: CFI_QUERY_WORDS * 2,
        },
    }
}

/// Leave ID-CFI mode (reset / ASO exit): write 0x00F0 to word 0.
pub const fn cfi_exit() -> IpCommand {
    IpCommand {
        address: 0,
        sequence: WRITE_DATA,
        transfer: Transfer::WriteWord(CMD_RESET_ASO_EXIT.to_be_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt4_recovery_resets_a_write_buffer_abort_then_clears_the_status() {
        let [u, reset, clear] = recover_steps();
        assert!(matches!(
            u,
            Step::Issue(IpCommand {
                sequence: UNLOCK,
                ..
            })
        ));
        for (step, cmd) in [(reset, 0x00F0u16), (clear, 0x0071)] {
            match step {
                Step::Issue(c) => {
                    assert_eq!(c.address, 0x555 * 2, "word 0x555");
                    assert_eq!(c.sequence, WRITE_DATA);
                    assert_eq!(c.transfer, Transfer::WriteWord(cmd.to_be_bytes()));
                }
                Step::WaitReady(_) => panic!("no waits"),
            }
        }
    }

    #[test]
    fn rt4_program_chunks_never_cross_the_write_buffer() {
        // Every offset parity class and lengths around the boundary.
        for offset in [0u32, 2, 510, 512, 1000, 0x3FF_FE00] {
            for len in [0usize, 2, 510, 512, 514, 1024, 3000] {
                let Ok(chunks) = program_chunks(offset, len) else {
                    assert!(offset as usize + len > FLASH_SIZE as usize);
                    continue;
                };
                let expected = chunks.len();
                let mut covered = 0usize;
                let mut at = offset;
                let mut n = 0;
                for c in chunks {
                    n += 1;
                    assert_eq!(c.offset(), at);
                    assert_eq!(c.data_start(), covered);
                    assert!(!c.is_empty() && c.len() as u32 <= WRITE_BUFFER_SIZE);
                    assert_eq!(c.len() % 2, 0);
                    let first = c.offset() / WRITE_BUFFER_SIZE;
                    let last = (c.offset() + c.len() as u32 - 1) / WRITE_BUFFER_SIZE;
                    assert_eq!(first, last, "chunk crosses a write buffer");
                    covered += c.len() as usize;
                    at += c.len() as u32;
                }
                assert_eq!(covered, len);
                assert_eq!(n, expected);
            }
        }
    }

    #[test]
    fn rt4_program_is_unlock_word_program_then_bounded_wait() {
        let chunk = program_chunks(0x1_0200, 4).ok().and_then(|mut c| c.next());
        let Some(chunk) = chunk else {
            panic!("one chunk expected")
        };
        let [a, b, c] = chunk.steps();
        assert_eq!(
            a,
            Step::Issue(IpCommand {
                address: 0x1_0200,
                sequence: UNLOCK,
                transfer: Transfer::None
            })
        );
        assert_eq!(
            b,
            Step::Issue(IpCommand {
                address: 0x1_0200,
                sequence: PROGRAM,
                transfer: Transfer::WriteData { start: 0, len: 4 },
            })
        );
        assert_eq!(c, Step::WaitReady(PROGRAM_WAIT));
    }

    #[test]
    fn rt4_program_refuses_odd_or_out_of_range() {
        assert_eq!(program_chunks(1, 2).err(), Some(PlanError::OddAlignment));
        assert_eq!(program_chunks(0, 3).err(), Some(PlanError::OddAlignment));
        assert_eq!(
            program_chunks(FLASH_SIZE - 2, 4).err(),
            Some(PlanError::OutOfRange)
        );
        assert_eq!(
            program_chunks(u32::MAX - 1, 2).err(),
            Some(PlanError::OutOfRange)
        );
        assert_eq!(
            program_chunks(0, usize::MAX).err(),
            Some(PlanError::OutOfRange)
        );
        assert!(program_chunks(FLASH_SIZE - 2, 2).is_ok());
    }

    #[test]
    fn rt4_erase_is_one_sector_per_command() {
        let Ok(s) = Sector::at(3 * SECTOR_SIZE) else {
            panic!("aligned")
        };
        let [a, b, c] = erase_sector_steps(s);
        assert_eq!(
            a,
            Step::Issue(IpCommand {
                address: 0xC_0000,
                sequence: UNLOCK,
                transfer: Transfer::None
            })
        );
        assert_eq!(
            b,
            Step::Issue(IpCommand {
                address: 0xC_0000,
                sequence: ERASE_SECTOR,
                transfer: Transfer::None,
            })
        );
        assert_eq!(c, Step::WaitReady(ERASE_WAIT));
        let Ok(sectors) = erase_sectors(SECTOR_SIZE, 2 * SECTOR_SIZE as usize) else {
            panic!("aligned range")
        };
        let idx: heapless::Vec<u16, 4> = sectors.map(Sector::index).collect();
        assert_eq!(idx.as_slice(), &[1, 2]);
    }

    #[test]
    fn rt4_erase_refuses_unaligned_or_out_of_range() {
        assert_eq!(Sector::at(0x1000), Err(PlanError::UnalignedSector));
        assert_eq!(Sector::at(FLASH_SIZE), Err(PlanError::OutOfRange));
        assert_eq!(Sector::new(256), None);
        assert_eq!(
            erase_sectors(0x1000, SECTOR_SIZE as usize).err(),
            Some(PlanError::UnalignedSector)
        );
        assert_eq!(
            erase_sectors(0, 0x1000).err(),
            Some(PlanError::UnalignedSector)
        );
        assert_eq!(
            erase_sectors(FLASH_SIZE - SECTOR_SIZE, 2 * SECTOR_SIZE as usize).err(),
            Some(PlanError::OutOfRange)
        );
        assert_eq!(
            erase_sectors(0, FLASH_SIZE as usize).map(|s| s.len()),
            Ok(256)
        );
    }

    #[test]
    fn rt4_cfi_and_read_commands() {
        let enter = cfi_enter();
        assert_eq!(enter.address, 0xAA);
        assert_eq!(enter.transfer, Transfer::WriteWord([0x00, 0x98]));
        assert_eq!(cfi_exit().transfer, Transfer::WriteWord([0x00, 0xF0]));
        assert_eq!(cfi_read().address, 0x20);
        assert_eq!(cfi_read().transfer, Transfer::Read { len: 96 });
        assert_eq!(read_status().transfer, Transfer::Read { len: 2 });
        assert_eq!(read(1, 2), Err(PlanError::OddAlignment));
        assert_eq!(read(0, 0), Err(PlanError::BadLength));
        assert_eq!(read(FLASH_SIZE - 2, 4), Err(PlanError::OutOfRange));
        assert!(read(0x100, 0xFFFF).is_ok());
    }
}
