//! The S26KS512S command sequences, in the FlexSPI LUT.
//!
//! HyperBus transactions start with a 48-bit command-address (CA) word:
//! CA\[47\] read (1) or write (0), CA\[46\] address space (0 = memory),
//! CA\[45\] burst type (1 = linear, 0 = wrapped), CA\[44:16\] the upper
//! word-address bits (`word >> 3`), CA\[2:0\] the lower ones (`word & 7`).
//! The flash's unlock and command cycles go to fixed word addresses (0x555,
//! 0x2AA), so they are encoded inline as six CMD_DDR bytes of CA plus two
//! bytes of data ([`command_cycle`]), one LUT sequence each; the cycle that
//! targets the caller's address uses RADDR/CADDR instead.
//!
//! The sequence layout follows NXP's MCUXpresso SDK example
//! `evkbimxrt1050/driver_examples/flexspi/hyper_flash/polling_transfer`
//! (`flexspi_hyper_flash_polling_transfer.c`, `customLUT`, Copyright
//! 2016 Freescale Semiconductor, Inc., Copyright 2016-2020 NXP,
//! BSD-3-Clause), whose words for status read, unlock, sector erase and
//! word program are reproduced exactly (the unit tests compare them).
//! Two places differ:
//!
//! * sequence 0 (read data) is the board's factory FCB read sequence
//!   (with 6 DDR dummy cycles), which the boot ROM loads for AHB reads, so
//!   AHB and IP reads use one sequence;
//! * the SDK's chip-erase sequences (12..15) are left out: nothing here
//!   may erase the bootloader block by accident.
//!
//! Programming uses the word program command (0xA0) followed by a write
//! burst of up to the 512-byte write buffer, as the SDK does; the
//! write-buffer commands (0x25 / 0x29) are not used.

use super::lut::{Instr, Lut, Op, Pads, SeqId, Sequence, SequenceRun};

/// HyperBus burst type, CA\[45\].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Burst {
    /// Wrapped burst (CA\[45\] = 0).
    Wrapped,
    /// Linear burst (CA\[45\] = 1).
    Linear,
}

/// Word address of the first unlock cycle and of command cycles.
pub const UNLOCK_ADDR_1: u32 = 0x555;
/// Word address of the second unlock cycle.
pub const UNLOCK_ADDR_2: u32 = 0x2AA;
/// Word address of the ID-CFI entry command.
pub const CFI_ENTRY_ADDR: u32 = 0x55;
/// Data of the first unlock cycle.
pub const UNLOCK_DATA_1: u16 = 0xAA;
/// Data of the second unlock cycle.
pub const UNLOCK_DATA_2: u16 = 0x55;
/// Status register read command.
pub const CMD_READ_STATUS: u16 = 0x70;
/// Word program command.
pub const CMD_WORD_PROGRAM: u16 = 0xA0;
/// Erase setup command.
pub const CMD_ERASE_SETUP: u16 = 0x80;
/// Sector erase command (to the sector address).
pub const CMD_SECTOR_ERASE: u16 = 0x30;
/// ID-CFI entry command.
pub const CMD_CFI_ENTRY: u16 = 0x98;
/// Clear Status Register command (to word [`UNLOCK_ADDR_1`]): clears the
/// sticky program, erase and write-buffer-abort error bits.
pub const CMD_CLEAR_STATUS: u16 = 0x71;
/// Reset / address-space (ASO, e.g. CFI) exit command.
pub const CMD_RESET_ASO_EXIT: u16 = 0xF0;

const fn cmd(byte: u8) -> Instr {
    Instr::ddr(Op::Cmd, Pads::Eight, byte)
}

/// One write cycle to a fixed word address: six CA bytes then two data
/// bytes (high byte first), all CMD_DDR on 8 pads.
pub const fn command_cycle(burst: Burst, word_addr: u32, data: u16) -> Sequence {
    let ca0 = match burst {
        Burst::Linear => 0x20,
        Burst::Wrapped => 0x00,
    };
    let upper = word_addr >> 3;
    let lower = word_addr & 0x7;
    Sequence::new([
        cmd(ca0 | ((upper >> 24) & 0x1F) as u8),
        cmd((upper >> 16) as u8),
        cmd((upper >> 8) as u8),
        cmd(upper as u8),
        cmd(0),
        cmd(lower as u8),
        cmd((data >> 8) as u8),
        cmd(data as u8),
    ])
}

const fn row() -> Instr {
    Instr::ddr(Op::RowAddr, Pads::Eight, 0x18)
}
const fn col() -> Instr {
    Instr::ddr(Op::ColumnAddr, Pads::Eight, 0x10)
}

const fn run(first: u8, count: u8) -> SequenceRun {
    match SequenceRun::new(first, count) {
        Some(r) => r,
        None => panic!("sequence run outside the LUT"),
    }
}

const fn id(i: u8) -> SeqId {
    match SeqId::new(i) {
        Some(s) => s,
        None => panic!("sequence index outside the LUT"),
    }
}

/// Read data (the factory FCB's read sequence): CA read, linear, from the
/// command address, 6 dummy cycles, then read.
pub const READ_DATA_SEQ: Sequence = Sequence::new([
    cmd(0xA0),
    row(),
    col(),
    Instr::ddr(Op::Dummy, Pads::Eight, 0x06),
    Instr::ddr(Op::Read, Pads::Eight, 0x04),
    Instr::STOP,
]);

/// Write one burst of data to the command address (CA write, linear).
/// Used for single-word commands at a chosen address (CFI entry, reset).
pub const WRITE_DATA_SEQ: Sequence = Sequence::new([
    cmd(0x20),
    row(),
    col(),
    Instr::ddr(Op::Write, Pads::Eight, 0x02),
]);

/// Read data (sequence 0).
pub const READ_DATA: SequenceRun = run(0, 1);
/// Write data at the command address (sequence 1).
pub const WRITE_DATA: SequenceRun = run(1, 1);
/// Status register read: 0x555 <- 0x70, then a read (sequences 2, 3).
pub const READ_STATUS: SequenceRun = run(2, 2);
/// The two unlock cycles (sequences 4, 5).
pub const UNLOCK: SequenceRun = run(4, 2);
/// Sector erase: 0x555 <- 0x80, the unlock cycles again, sector <- 0x30
/// (sequences 6..9). Preceded by [`UNLOCK`].
pub const ERASE_SECTOR: SequenceRun = run(6, 4);
/// Word program: 0x555 <- 0xA0, then the data burst to the command
/// address (sequences 10, 11). Preceded by [`UNLOCK`].
pub const PROGRAM: SequenceRun = run(10, 2);

/// The full LUT the HyperFlash driver loads.
pub const LUT: Lut = Lut::EMPTY
    .with(id(0), READ_DATA_SEQ)
    .with(id(1), WRITE_DATA_SEQ)
    .with(
        id(2),
        command_cycle(Burst::Wrapped, UNLOCK_ADDR_1, CMD_READ_STATUS),
    )
    .with(
        id(3),
        Sequence::new([
            cmd(0xA0),
            row(),
            col(),
            Instr::ddr(Op::DummyRwds, Pads::Eight, 0x0B),
            Instr::ddr(Op::Read, Pads::Eight, 0x04),
            Instr::STOP,
        ]),
    )
    .with(
        id(4),
        command_cycle(Burst::Linear, UNLOCK_ADDR_1, UNLOCK_DATA_1),
    )
    .with(
        id(5),
        command_cycle(Burst::Linear, UNLOCK_ADDR_2, UNLOCK_DATA_2),
    )
    .with(
        id(6),
        command_cycle(Burst::Wrapped, UNLOCK_ADDR_1, CMD_ERASE_SETUP),
    )
    .with(
        id(7),
        command_cycle(Burst::Wrapped, UNLOCK_ADDR_1, UNLOCK_DATA_1),
    )
    .with(
        id(8),
        command_cycle(Burst::Wrapped, UNLOCK_ADDR_2, UNLOCK_DATA_2),
    )
    .with(
        id(9),
        Sequence::new([
            cmd(0x00),
            row(),
            col(),
            cmd(0x00),
            cmd(CMD_SECTOR_ERASE as u8),
            Instr::STOP,
        ]),
    )
    .with(
        id(10),
        command_cycle(Burst::Linear, UNLOCK_ADDR_1, CMD_WORD_PROGRAM),
    )
    .with(
        id(11),
        Sequence::new([
            cmd(0x20),
            row(),
            col(),
            Instr::ddr(Op::Write, Pads::Eight, 0x80),
        ]),
    );

/// The LUT that goes in the FCB: only the read sequence, as the factory
/// FCB carries.
pub const FCB_LUT: Lut = Lut::EMPTY.with(id(0), READ_DATA_SEQ);

#[cfg(test)]
mod tests {
    use super::*;

    /// LUT words 4..47 of the SDK example's `customLUT`, computed from its
    /// `FLEXSPI_LUT_SEQ` macro invocations (index -> word); word 4..5 is
    /// its write-data sequence, 8..47 status, write enable (unlock), erase
    /// sector and page program.
    const SDK_WORDS: [(usize, u32); 37] = [
        (4, 0x8B18_8720),
        (5, 0xA302_8F10),
        (8, 0x8700_8700),
        (9, 0x87AA_8700),
        (10, 0x8705_8700),
        (11, 0x8770_8700),
        (12, 0x8B18_87A0),
        (13, 0xB70B_8F10),
        (14, 0x0000_A704),
        (16, 0x8700_8720),
        (17, 0x87AA_8700),
        (18, 0x8705_8700),
        (19, 0x87AA_8700),
        (20, 0x8700_8720),
        (21, 0x8755_8700),
        (22, 0x8702_8700),
        (23, 0x8755_8700),
        (24, 0x8700_8700),
        (25, 0x87AA_8700),
        (26, 0x8705_8700),
        (27, 0x8780_8700),
        (28, 0x8700_8700),
        (29, 0x87AA_8700),
        (30, 0x8705_8700),
        (31, 0x87AA_8700),
        (32, 0x8700_8700),
        (33, 0x8755_8700),
        (34, 0x8702_8700),
        (35, 0x8755_8700),
        (36, 0x8B18_8700),
        (37, 0x8700_8F10),
        (38, 0x0000_8730),
        (40, 0x8700_8720),
        (41, 0x87AA_8700),
        (42, 0x8705_8700),
        (43, 0x87A0_8700),
        (44, 0x8B18_8720),
    ];

    #[test]
    fn rt4_lut_matches_the_sdk_hyperflash_sequences() {
        let words = LUT.words();
        for (i, w) in SDK_WORDS {
            assert_eq!(words[i], w, "LUT word {i}");
        }
        assert_eq!(words[45], 0xA380_8F10);
        // Words the SDK leaves zero inside those sequences.
        for i in [6, 7, 15, 39, 46, 47] {
            assert_eq!(words[i], 0, "LUT word {i}");
        }
    }

    #[test]
    fn rt4_read_sequence_is_the_factory_one() {
        assert_eq!(
            &LUT.words()[0..4],
            &[0x8B18_87A0, 0xB306_8F10, 0x0000_A704, 0]
        );
    }

    #[test]
    fn rt4_no_chip_erase_in_the_lut() {
        assert!(LUT.words()[48..].iter().all(|&w| w == 0));
    }

    #[test]
    fn rt4_unlock_cycles_address_0x555_and_0x2aa() {
        let first = command_cycle(Burst::Wrapped, UNLOCK_ADDR_1, UNLOCK_DATA_1);
        let ops: [u16; 8] = [0, 0, 0, 0xAA, 0, 0x05, 0, 0xAA];
        for (instr, op) in first.instrs().iter().zip(ops) {
            assert_eq!(instr.bits(), 0x8700 | op);
        }
        let second = command_cycle(Burst::Wrapped, UNLOCK_ADDR_2, UNLOCK_DATA_2);
        let ops: [u16; 8] = [0, 0, 0, 0x55, 0, 0x02, 0, 0x55];
        for (instr, op) in second.instrs().iter().zip(ops) {
            assert_eq!(instr.bits(), 0x8700 | op);
        }
    }

    #[test]
    fn rt4_sequence_runs_do_not_overlap() {
        let runs = [
            READ_DATA,
            WRITE_DATA,
            READ_STATUS,
            UNLOCK,
            ERASE_SECTOR,
            PROGRAM,
        ];
        let mut used = [false; 16];
        for r in runs {
            for s in r.first().index()..r.first().index() + r.count() {
                assert!(!used[s as usize], "sequence {s} used twice");
                used[s as usize] = true;
            }
        }
    }
}
