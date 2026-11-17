//! The FlexSPI lookup-table (LUT) instruction format, as typed values.
//!
//! The FlexSPI controller runs flash transactions from a LUT of 16
//! sequences. A sequence is 8 instructions packed two per 32-bit word (4
//! words); the whole LUT is 64 words. One instruction is 16 bits:
//!
//! ```text
//! bits 15..10  opcode (SDR value, or SDR value + 0x20 for DDR)
//! bits  9..8   pads (0 = 1 pad, 1 = 2, 2 = 4, 3 = 8)
//! bits  7..0   operand
//! ```
//!
//! The even instruction of a pair sits in the low half of the word. An
//! instruction is built only through [`Instr::sdr`], [`Instr::ddr`],
//! [`Instr::STOP`] and [`Instr::jump_on_cs`], so an opcode that has no DDR
//! form (STOP, JMP_ON_CS) cannot be encoded as DDR. Unused slots are STOP
//! (all zero), which ends a sequence.
//!
//! Opcode values are those of the i.MX RT1050 reference manual's FlexSPI
//! chapter, also used by the `imxrt-boot-gen` crate and NXP's `fsl_flexspi`
//! driver.

/// The opcodes that exist in both SDR and DDR form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Transmit the operand as a command byte.
    Cmd,
    /// Transmit the row address; operand = number of address bits.
    RowAddr,
    /// Transmit the column address; operand = number of address bits.
    ColumnAddr,
    /// Transmit the mode bits, 1 bit wide.
    Mode1,
    /// Transmit the mode bits, 2 bits wide.
    Mode2,
    /// Transmit the mode bits, 4 bits wide.
    Mode4,
    /// Transmit the mode bits, 8 bits wide.
    Mode8,
    /// Transmit the programmed data (length from the IP command or AHB).
    Write,
    /// Receive data (length from the IP command or AHB).
    Read,
    /// Receive the read-learning pattern.
    Learn,
    /// Transmit the data size.
    DataSize,
    /// Wait the operand's number of dummy cycles.
    Dummy,
    /// Dummy cycles, doubled when RWDS signals extra latency (HyperBus).
    DummyRwds,
}

impl Op {
    /// The SDR opcode value.
    const fn sdr_code(self) -> u16 {
        match self {
            Op::Cmd => 0x01,
            Op::RowAddr => 0x02,
            Op::ColumnAddr => 0x03,
            Op::Mode1 => 0x04,
            Op::Mode2 => 0x05,
            Op::Mode4 => 0x06,
            Op::Mode8 => 0x07,
            Op::Write => 0x08,
            Op::Read => 0x09,
            Op::Learn => 0x0A,
            Op::DataSize => 0x0B,
            Op::Dummy => 0x0C,
            Op::DummyRwds => 0x0D,
        }
    }
}

/// Number of data pads an instruction drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pads {
    /// 1 pad (single).
    One = 0,
    /// 2 pads (dual).
    Two = 1,
    /// 4 pads (quad).
    Four = 2,
    /// 8 pads (octal; HyperBus).
    Eight = 3,
}

/// One encoded LUT instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instr(u16);

/// The JMP_ON_CS opcode (SDR only).
const JUMP_ON_CS: u16 = 0x1F;
/// Added to an SDR opcode to make its DDR form.
const DDR_OFFSET: u16 = 0x20;

impl Instr {
    /// STOP: ends the sequence (encodes as 0).
    pub const STOP: Instr = Instr(0);

    /// An SDR instruction.
    pub const fn sdr(op: Op, pads: Pads, operand: u8) -> Instr {
        Instr::raw(op.sdr_code(), pads, operand)
    }

    /// A DDR instruction (SDR opcode + 0x20).
    pub const fn ddr(op: Op, pads: Pads, operand: u8) -> Instr {
        Instr::raw(op.sdr_code() + DDR_OFFSET, pads, operand)
    }

    /// JMP_ON_CS: jump to instruction `operand` when CS is deasserted.
    pub const fn jump_on_cs(operand: u8) -> Instr {
        Instr::raw(JUMP_ON_CS, Pads::One, operand)
    }

    const fn raw(opcode: u16, pads: Pads, operand: u8) -> Instr {
        Instr((opcode << 10) | ((pads as u16) << 8) | operand as u16)
    }

    /// The 16-bit encoding.
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// Instructions in one sequence.
pub const INSTRS_PER_SEQUENCE: usize = 8;
/// 32-bit words in one sequence.
pub const WORDS_PER_SEQUENCE: usize = 4;
/// Sequences in the LUT.
pub const SEQUENCES: usize = 16;
/// 32-bit words in the LUT.
pub const LUT_WORDS: usize = SEQUENCES * WORDS_PER_SEQUENCE;

/// One LUT sequence: 8 instructions, unused ones STOP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequence([Instr; INSTRS_PER_SEQUENCE]);

impl Sequence {
    /// An all-STOP sequence (an unused LUT slot).
    pub const EMPTY: Sequence = Sequence([Instr::STOP; INSTRS_PER_SEQUENCE]);

    /// A sequence of up to 8 instructions, padded with STOP. More than 8
    /// is a compile-time error.
    pub const fn new<const N: usize>(instrs: [Instr; N]) -> Sequence {
        const {
            assert!(
                N <= INSTRS_PER_SEQUENCE,
                "a LUT sequence holds at most 8 instructions"
            )
        };
        let mut out = [Instr::STOP; INSTRS_PER_SEQUENCE];
        let mut i = 0;
        while i < N {
            out[i] = instrs[i];
            i += 1;
        }
        Sequence(out)
    }

    /// The instructions.
    pub const fn instrs(&self) -> &[Instr; INSTRS_PER_SEQUENCE] {
        &self.0
    }

    /// The 4 LUT words: instruction `2k` in the low half of word `k`.
    pub const fn words(&self) -> [u32; WORDS_PER_SEQUENCE] {
        let mut out = [0u32; WORDS_PER_SEQUENCE];
        let mut k = 0;
        while k < WORDS_PER_SEQUENCE {
            out[k] = self.0[2 * k].bits() as u32 | ((self.0[2 * k + 1].bits() as u32) << 16);
            k += 1;
        }
        out
    }
}

/// A LUT sequence index, `0..16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqId(u8);

impl SeqId {
    /// The index, or `None` past the 16 sequences.
    pub const fn new(index: u8) -> Option<SeqId> {
        if (index as usize) < SEQUENCES {
            Some(SeqId(index))
        } else {
            None
        }
    }

    /// The index.
    pub const fn index(self) -> u8 {
        self.0
    }
}

/// Consecutive sequences one IP command runs (IPCR1's ISEQID and
/// ISEQNUM = `count - 1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceRun {
    first: SeqId,
    count: u8,
}

impl SequenceRun {
    /// `count` (1..=8, ISEQNUM is 3 bits) sequences from `first`, all
    /// inside the LUT; `None` otherwise.
    pub const fn new(first: u8, count: u8) -> Option<SequenceRun> {
        if count == 0 || count > 8 || first as usize + count as usize > SEQUENCES {
            return None;
        }
        match SeqId::new(first) {
            Some(first) => Some(SequenceRun { first, count }),
            None => None,
        }
    }

    /// The first sequence.
    pub const fn first(self) -> SeqId {
        self.first
    }

    /// How many sequences run.
    pub const fn count(self) -> u8 {
        self.count
    }

    /// IPCR1's ISEQNUM field (`count - 1`).
    pub const fn iseqnum(self) -> u8 {
        self.count - 1
    }
}

/// A whole LUT: 16 sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lut([Sequence; SEQUENCES]);

impl Lut {
    /// A LUT with every sequence empty.
    pub const EMPTY: Lut = Lut([Sequence::EMPTY; SEQUENCES]);

    /// This LUT with `seq` placed at `id`.
    pub const fn with(self, id: SeqId, seq: Sequence) -> Lut {
        let mut table = self.0;
        table[id.0 as usize] = seq;
        Lut(table)
    }

    /// The sequence at `id`.
    pub const fn sequence(&self, id: SeqId) -> &Sequence {
        &self.0[id.0 as usize]
    }

    /// The 64 LUT words, sequence 0 first.
    pub const fn words(&self) -> [u32; LUT_WORDS] {
        let mut out = [0u32; LUT_WORDS];
        let mut s = 0;
        while s < SEQUENCES {
            let w = self.0[s].words();
            let mut k = 0;
            while k < WORDS_PER_SEQUENCE {
                out[s * WORDS_PER_SEQUENCE + k] = w[k];
                k += 1;
            }
            s += 1;
        }
        out
    }

    /// The LUT as 256 little-endian bytes (its layout in the FCB).
    pub const fn to_le_bytes(&self) -> [u8; LUT_WORDS * 4] {
        let words = self.words();
        let mut out = [0u8; LUT_WORDS * 4];
        let mut i = 0;
        while i < LUT_WORDS {
            let b = words[i].to_le_bytes();
            out[4 * i] = b[0];
            out[4 * i + 1] = b[1];
            out[4 * i + 2] = b[2];
            out[4 * i + 3] = b[3];
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt4_instruction_packs_opcode_pads_operand() {
        // CMD_DDR (0x21), 8 pads (3), 0xA0 -> 0x21<<10 | 3<<8 | 0xA0.
        assert_eq!(Instr::ddr(Op::Cmd, Pads::Eight, 0xA0).bits(), 0x87A0);
        assert_eq!(Instr::sdr(Op::Cmd, Pads::One, 0xEB).bits(), 0x04EB);
        assert_eq!(Instr::ddr(Op::Read, Pads::Eight, 0x04).bits(), 0xA704);
        assert_eq!(Instr::STOP.bits(), 0);
        assert_eq!(Instr::jump_on_cs(0).bits(), 0x7C00);
    }

    #[test]
    fn rt4_two_instructions_per_word_even_in_low_half() {
        let seq = Sequence::new([
            Instr::ddr(Op::Cmd, Pads::Eight, 0xA0),
            Instr::ddr(Op::RowAddr, Pads::Eight, 0x18),
        ]);
        assert_eq!(seq.words(), [0x8B18_87A0, 0, 0, 0]);
    }

    #[test]
    fn rt4_sequence_run_refuses_out_of_lut() {
        assert!(SequenceRun::new(15, 1).is_some());
        assert!(SequenceRun::new(15, 2).is_none());
        assert!(SequenceRun::new(0, 0).is_none());
        assert!(SequenceRun::new(16, 1).is_none());
        assert!(SeqId::new(16).is_none());
        assert_eq!(SequenceRun::new(6, 4).map(SequenceRun::iseqnum), Some(3));
    }
}
