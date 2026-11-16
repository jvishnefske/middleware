//! Device configuration data (DCD): the boot ROM's register script.
//!
//! The boot ROM executes the DCD the IVT points to before it jumps to the
//! image; here it sets up the SDRAM ([`evkb_sdram`](super::evkb_sdram)).
//! The encoding (i.MX RT1050 reference manual, "Device Configuration Data
//! (DCD)"; the HAB command format) is big-endian throughout:
//!
//! * header: tag `0xD2`, total length (u16, header included), version
//!   `0x41`;
//! * write data: tag `0xCC`, command length (u16), parameter, then
//!   (address, value) pairs of u32;
//! * check data: tag `0xCF`, length 12 (16 with a poll count), parameter,
//!   address, mask \[, count\];
//! * NOP: `0xC0 0x00 0x04 0x00`.
//!
//! The parameter byte is the access width in bytes (1, 2, 4) in bits 2..0
//! and two flags: bit 3 "mask" and bit 4 "set". For write data they select
//! write value (0, 0), clear bits (1, 0) or set bits (1, 1); for check
//! data, all bits clear (0, 0), all set (0, 1), any clear (1, 0) or any
//! set (1, 1).
//!
//! [`Dcd::encode`] refuses an empty write list, an address not aligned to
//! its width, a value or mask wider than the width, and a DCD larger than
//! the boot ROM's 1768-byte limit ([`DCD_MAX_LEN`]).

/// The boot ROM's DCD size limit (i.MX RT1050 reference manual).
pub const DCD_MAX_LEN: usize = 1768;

const TAG_DCD: u8 = 0xD2;
const TAG_WRITE: u8 = 0xCC;
const TAG_CHECK: u8 = 0xCF;
const TAG_NOP: u8 = 0xC0;
/// DCD header version.
pub const DCD_VERSION: u8 = 0x41;
const FLAG_MASK: u8 = 1 << 3;
const FLAG_SET: u8 = 1 << 4;

/// Register access width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// 8 bits.
    Byte = 1,
    /// 16 bits.
    Half = 2,
    /// 32 bits.
    Word = 4,
}

impl Width {
    const fn holds(self, value: u32) -> bool {
        match self {
            Width::Byte => value <= 0xFF,
            Width::Half => value <= 0xFFFF,
            Width::Word => true,
        }
    }
}

/// What a write-data command does with each value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOp {
    /// `*address = value`.
    Write,
    /// `*address &= !value`.
    ClearBits,
    /// `*address |= value`.
    SetBits,
}

/// What a check-data command waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckCond {
    /// `(*address & mask) == 0`.
    AllClear,
    /// `(*address & mask) == mask`.
    AllSet,
    /// `(*address & mask) != mask`.
    AnyClear,
    /// `(*address & mask) != 0`.
    AnySet,
}

/// One register write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterWrite {
    /// Register address.
    pub address: u32,
    /// Value (or bit mask for clear / set).
    pub value: u32,
}

/// One DCD command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DcdCommand<'a> {
    /// Write data: every entry with the same op and width.
    Write {
        /// What to do with each value.
        op: WriteOp,
        /// Access width.
        width: Width,
        /// The writes, in order.
        writes: &'a [RegisterWrite],
    },
    /// Check data: poll until the condition holds (`count` polls at most
    /// when given, otherwise until it holds).
    Check {
        /// The condition.
        cond: CheckCond,
        /// Access width.
        width: Width,
        /// Register address.
        address: u32,
        /// Bits checked.
        mask: u32,
        /// Optional poll count.
        count: Option<u32>,
    },
    /// No operation.
    Nop,
}

/// Why a DCD was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DcdError {
    /// No commands, or a write command with no writes.
    Empty,
    /// An address not aligned to its access width.
    Misaligned(u32),
    /// A value or mask wider than its access width.
    ValueTooWide(u32),
    /// Larger than [`DCD_MAX_LEN`].
    TooLong,
}

/// An encoded DCD, at most [`DCD_MAX_LEN`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dcd {
    bytes: [u8; DCD_MAX_LEN],
    len: usize,
}

/// Bounded writer into the DCD buffer.
struct Out {
    bytes: [u8; DCD_MAX_LEN],
    len: usize,
}

impl Out {
    fn put(&mut self, data: &[u8]) -> Result<(), DcdError> {
        let end = self.len.checked_add(data.len()).ok_or(DcdError::TooLong)?;
        let dst = self.bytes.get_mut(self.len..end).ok_or(DcdError::TooLong)?;
        dst.copy_from_slice(data);
        self.len = end;
        Ok(())
    }

    fn header(&mut self, tag: u8, len: usize, param: u8) -> Result<(), DcdError> {
        let len = u16::try_from(len).map_err(|_| DcdError::TooLong)?;
        let l = len.to_be_bytes();
        self.put(&[tag, l[0], l[1], param])
    }
}

fn check_access(width: Width, address: u32, value: u32) -> Result<(), DcdError> {
    if !address.is_multiple_of(width as u32) {
        return Err(DcdError::Misaligned(address));
    }
    if !width.holds(value) {
        return Err(DcdError::ValueTooWide(value));
    }
    Ok(())
}

impl Dcd {
    /// Encode `commands` behind a DCD header.
    pub fn encode(commands: &[DcdCommand<'_>]) -> Result<Dcd, DcdError> {
        if commands.is_empty() {
            return Err(DcdError::Empty);
        }
        let mut out = Out {
            bytes: [0; DCD_MAX_LEN],
            len: 4,
        };
        for cmd in commands {
            match *cmd {
                DcdCommand::Write { op, width, writes } => {
                    if writes.is_empty() {
                        return Err(DcdError::Empty);
                    }
                    let flags = match op {
                        WriteOp::Write => 0,
                        WriteOp::ClearBits => FLAG_MASK,
                        WriteOp::SetBits => FLAG_MASK | FLAG_SET,
                    };
                    let len = writes.len().checked_mul(8).and_then(|n| n.checked_add(4));
                    out.header(
                        TAG_WRITE,
                        len.ok_or(DcdError::TooLong)?,
                        width as u8 | flags,
                    )?;
                    for w in writes {
                        check_access(width, w.address, w.value)?;
                        out.put(&w.address.to_be_bytes())?;
                        out.put(&w.value.to_be_bytes())?;
                    }
                }
                DcdCommand::Check {
                    cond,
                    width,
                    address,
                    mask,
                    count,
                } => {
                    check_access(width, address, mask)?;
                    let flags = match cond {
                        CheckCond::AllClear => 0,
                        CheckCond::AllSet => FLAG_SET,
                        CheckCond::AnyClear => FLAG_MASK,
                        CheckCond::AnySet => FLAG_MASK | FLAG_SET,
                    };
                    let len = if count.is_some() { 16 } else { 12 };
                    out.header(TAG_CHECK, len, width as u8 | flags)?;
                    out.put(&address.to_be_bytes())?;
                    out.put(&mask.to_be_bytes())?;
                    if let Some(c) = count {
                        out.put(&c.to_be_bytes())?;
                    }
                }
                DcdCommand::Nop => out.header(TAG_NOP, 4, 0)?,
            }
        }
        let total = out.len;
        let mut header = Out {
            bytes: out.bytes,
            len: 0,
        };
        header.header(TAG_DCD, total, DCD_VERSION)?;
        Ok(Dcd {
            bytes: header.bytes,
            len: total,
        })
    }

    /// The encoded bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }

    /// Encoded length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Never true: an encoded DCD has at least its header.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt2_dcd_header_and_commands_are_big_endian() {
        let writes = [RegisterWrite {
            address: 0x402F_0000,
            value: 0x1000_0004,
        }];
        let cmds = [
            DcdCommand::Write {
                op: WriteOp::Write,
                width: Width::Word,
                writes: &writes,
            },
            DcdCommand::Check {
                cond: CheckCond::AnySet,
                width: Width::Word,
                address: 0x402F_003C,
                mask: 1,
                count: None,
            },
            DcdCommand::Nop,
        ];
        let Ok(dcd) = Dcd::encode(&cmds) else {
            panic!("valid DCD")
        };
        assert_eq!(
            dcd.as_bytes(),
            &[
                0xD2, 0x00, 0x20, 0x41, // header, 32 bytes
                0xCC, 0x00, 0x0C, 0x04, 0x40, 0x2F, 0x00, 0x00, 0x10, 0x00, 0x00, 0x04, //
                0xCF, 0x00, 0x0C, 0x1C, 0x40, 0x2F, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x01, //
                0xC0, 0x00, 0x04, 0x00,
            ]
        );
    }

    #[test]
    fn rt2_dcd_flags_and_poll_count() {
        let writes = [RegisterWrite {
            address: 0x10,
            value: 0x3,
        }];
        let set = [DcdCommand::Write {
            op: WriteOp::SetBits,
            width: Width::Byte,
            writes: &writes,
        }];
        assert_eq!(Dcd::encode(&set).map(|d| d.as_bytes()[7]), Ok(0x19));
        let clear = [DcdCommand::Write {
            op: WriteOp::ClearBits,
            width: Width::Half,
            writes: &writes,
        }];
        assert_eq!(Dcd::encode(&clear).map(|d| d.as_bytes()[7]), Ok(0x0A));
        let check = [DcdCommand::Check {
            cond: CheckCond::AllSet,
            width: Width::Word,
            address: 0x20,
            mask: 0x80,
            count: Some(100),
        }];
        let Ok(d) = Dcd::encode(&check) else {
            panic!("valid DCD")
        };
        assert_eq!(&d.as_bytes()[4..8], &[0xCF, 0x00, 0x10, 0x14]);
        assert_eq!(&d.as_bytes()[16..20], &[0, 0, 0, 100]);
    }

    #[test]
    fn rt2_dcd_refuses_bad_commands() {
        assert_eq!(Dcd::encode(&[]), Err(DcdError::Empty));
        let none = [DcdCommand::Write {
            op: WriteOp::Write,
            width: Width::Word,
            writes: &[],
        }];
        assert_eq!(Dcd::encode(&none), Err(DcdError::Empty));
        let odd = [RegisterWrite {
            address: 0x402F_0002,
            value: 0,
        }];
        let mis = [DcdCommand::Write {
            op: WriteOp::Write,
            width: Width::Word,
            writes: &odd,
        }];
        assert_eq!(Dcd::encode(&mis), Err(DcdError::Misaligned(0x402F_0002)));
        let wide = [RegisterWrite {
            address: 0x10,
            value: 0x100,
        }];
        let w = [DcdCommand::Write {
            op: WriteOp::Write,
            width: Width::Byte,
            writes: &wide,
        }];
        assert_eq!(Dcd::encode(&w), Err(DcdError::ValueTooWide(0x100)));
        let many = [RegisterWrite {
            address: 0,
            value: 0,
        }; 221];
        let big = [DcdCommand::Write {
            op: WriteOp::Write,
            width: Width::Word,
            writes: &many,
        }];
        assert_eq!(Dcd::encode(&big), Err(DcdError::TooLong));
        // 220 writes: 4 + 4 + 1760 = 1768 bytes, exactly the limit.
        let fits = [DcdCommand::Write {
            op: WriteOp::Write,
            width: Width::Word,
            writes: &many[..220],
        }];
        assert_eq!(Dcd::encode(&fits).map(|d| d.len()), Ok(DCD_MAX_LEN));
    }
}
