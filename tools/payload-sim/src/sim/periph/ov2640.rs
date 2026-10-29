//! An OV2640's SCCB register file (no image sensor): what a payload sees
//! when it sets the camera up over I2C bus 0 at [`OV2640_ADDR`].
//!
//! * Two banks of 256 registers. Writing `0xFF` (`BANK_SEL`) selects the
//!   DSP bank (bit 0 clear) or the sensor bank (bit 0 set); every other
//!   address is read and written in the selected bank.
//! * The sensor bank answers `PID` (0x0A) = [`PID`] and `VER` (0x0B) =
//!   [`VER`]; both are read-only.
//! * Writing `COM7` (0x12) in the sensor bank with bit 7 set is a soft
//!   reset: every register goes back to its default (0, except `PID` and
//!   `VER`), the DSP bank is selected, and the reset bit clears itself.
//! * A transfer writes `tx` first: `[reg]` sets the register pointer,
//!   `[reg, value]` also stores `value` there (one register per
//!   transaction, as SCCB writes); a longer write is not acknowledged
//!   ([`AbiError::Transport`]). Then it reads `rx` from the pointer,
//!   auto-incrementing, so a register read is `tx = [reg]` followed by a
//!   separate `rx = [_]` transfer (SCCB has no repeated start).
//!
//! The model counts register writes (bank selects included) and soft
//! resets. The frames themselves are out of scope: the simulator's camera
//! route is `Unsupported`.

use wasm_host::AbiError;

/// The OV2640's 7-bit SCCB address.
pub const OV2640_ADDR: u8 = 0x30;
/// `PID` as the model answers it.
pub const PID: u8 = 0x26;
/// `VER` as the model answers it.
pub const VER: u8 = 0x42;

/// Bank select register (both banks).
const BANK_SEL: u8 = 0xFF;
/// `PID` register (sensor bank).
const REG_PID: u8 = 0x0A;
/// `VER` register (sensor bank).
const REG_VER: u8 = 0x0B;
/// `COM7` register (sensor bank); bit 7 is the soft reset.
const COM7: u8 = 0x12;
const COM7_SRST: u8 = 0x80;

/// A register bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bank {
    /// `BANK_SEL` = 0.
    Dsp = 0,
    /// `BANK_SEL` = 1.
    Sensor = 1,
}

/// The register model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ov2640 {
    regs: [[u8; 256]; 2],
    bank: Bank,
    pointer: u8,
    writes: u32,
    resets: u32,
}

impl Default for Ov2640 {
    fn default() -> Self {
        Ov2640 {
            regs: Self::defaults(),
            bank: Bank::Dsp,
            pointer: 0,
            writes: 0,
            resets: 0,
        }
    }
}

impl Ov2640 {
    fn defaults() -> [[u8; 256]; 2] {
        let mut regs = [[0u8; 256]; 2];
        regs[Bank::Sensor as usize][usize::from(REG_PID)] = PID;
        regs[Bank::Sensor as usize][usize::from(REG_VER)] = VER;
        regs
    }

    /// One SCCB transaction: write `tx`, then read `rx` (see the module
    /// docs).
    pub fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), AbiError> {
        match *tx {
            [] => {}
            [reg] => self.pointer = reg,
            [reg, value] => {
                self.pointer = reg;
                self.write(reg, value);
            }
            _ => return Err(AbiError::Transport),
        }
        for (i, out) in rx.iter_mut().enumerate() {
            *out = self.reg(self.bank, self.pointer.wrapping_add(i as u8));
        }
        Ok(())
    }

    fn write(&mut self, reg: u8, value: u8) {
        self.writes = self.writes.saturating_add(1);
        if reg == BANK_SEL {
            self.bank = if value & 1 == 0 {
                Bank::Dsp
            } else {
                Bank::Sensor
            };
            return;
        }
        if self.bank == Bank::Sensor {
            if reg == COM7 && value & COM7_SRST != 0 {
                self.regs = Self::defaults();
                self.bank = Bank::Dsp;
                self.resets = self.resets.saturating_add(1);
                return;
            }
            if reg == REG_PID || reg == REG_VER {
                return;
            }
        }
        self.regs[self.bank as usize][usize::from(reg)] = value;
    }

    /// Register `reg` of `bank` (`BANK_SEL` reads back the selected bank).
    pub fn reg(&self, bank: Bank, reg: u8) -> u8 {
        if reg == BANK_SEL {
            self.bank as u8
        } else {
            self.regs[bank as usize][usize::from(reg)]
        }
    }

    /// Register writes so far, bank selects and resets included.
    pub fn writes(&self) -> u32 {
        self.writes
    }

    /// Soft resets so far.
    pub fn resets(&self) -> u32 {
        self.resets
    }

    /// The registers of `bank` that differ from their defaults, as
    /// `(reg, value)` in address order.
    pub fn changed(&self, bank: Bank) -> Vec<(u8, u8)> {
        let defaults = Self::defaults();
        (0..=255u8)
            .filter(|&r| r != BANK_SEL)
            .map(|r| (r, self.regs[bank as usize][usize::from(r)]))
            .filter(|&(r, v)| v != defaults[bank as usize][usize::from(r)])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register read as a payload does it: address, then one byte.
    fn read(m: &mut Ov2640, reg: u8) -> u8 {
        m.transfer(&[reg], &mut []).unwrap();
        let mut b = [0u8];
        m.transfer(&[], &mut b).unwrap();
        b[0]
    }

    #[test]
    fn answers_the_id_in_the_sensor_bank_only() {
        let mut m = Ov2640::default();
        assert_eq!(m.reg(Bank::Dsp, 0xFF), Bank::Dsp as u8);
        assert_eq!(read(&mut m, 0x0A), 0, "DSP bank");
        m.transfer(&[0xFF, 0x01], &mut []).unwrap();
        assert_eq!(m.reg(Bank::Dsp, 0xFF), Bank::Sensor as u8);
        assert_eq!(read(&mut m, 0x0A), PID);
        assert_eq!(read(&mut m, 0x0B), VER);
        // Read-only.
        m.transfer(&[0x0A, 0x00], &mut []).unwrap();
        assert_eq!(read(&mut m, 0x0A), PID);
        // A read with the address in the same transfer works too.
        let mut two = [0u8; 2];
        m.transfer(&[0x0A], &mut two).unwrap();
        assert_eq!(two, [PID, VER]);
    }

    #[test]
    fn banks_hold_separate_registers() {
        let mut m = Ov2640::default();
        m.transfer(&[0xFF, 0x00], &mut []).unwrap();
        m.transfer(&[0x44, 0x0C], &mut []).unwrap();
        m.transfer(&[0xFF, 0x01], &mut []).unwrap();
        m.transfer(&[0x44, 0x99], &mut []).unwrap();
        assert_eq!(m.reg(Bank::Dsp, 0x44), 0x0C);
        assert_eq!(m.reg(Bank::Sensor, 0x44), 0x99);
        assert_eq!(read(&mut m, 0x44), 0x99);
        assert_eq!(read(&mut m, 0xFF), 1, "BANK_SEL reads back");
        assert_eq!(m.writes(), 4);
        assert_eq!(m.changed(Bank::Dsp), vec![(0x44, 0x0C)]);
        assert_eq!(m.changed(Bank::Sensor), vec![(0x44, 0x99)]);
    }

    #[test]
    fn com7_bit_7_in_the_sensor_bank_is_a_soft_reset() {
        let mut m = Ov2640::default();
        m.transfer(&[0xFF, 0x00], &mut []).unwrap();
        m.transfer(&[0xDA, 0x10], &mut []).unwrap();
        // COM7's address in the DSP bank is an ordinary register.
        m.transfer(&[0x12, 0x80], &mut []).unwrap();
        assert_eq!(m.resets(), 0);
        assert_eq!(m.reg(Bank::Dsp, 0x12), 0x80);
        m.transfer(&[0xFF, 0x01], &mut []).unwrap();
        m.transfer(&[0x03, 0x0F], &mut []).unwrap();
        m.transfer(&[0x12, 0x80], &mut []).unwrap();
        assert_eq!(m.resets(), 1);
        assert_eq!(m.reg(Bank::Sensor, 0xFF), Bank::Dsp as u8, "DSP bank");
        assert_eq!(m.reg(Bank::Dsp, 0xDA), 0);
        assert_eq!(m.reg(Bank::Sensor, 0x03), 0);
        assert_eq!(m.reg(Bank::Sensor, 0x12), 0, "self-clearing");
        assert_eq!(m.reg(Bank::Sensor, 0x0A), PID, "the ID survives");
        assert!(m.changed(Bank::Dsp).is_empty() && m.changed(Bank::Sensor).is_empty());
        assert_eq!(m.writes(), 6, "counted across the reset");
        // COM7 without the reset bit is stored.
        m.transfer(&[0xFF, 0x01], &mut []).unwrap();
        m.transfer(&[0x12, 0x00], &mut []).unwrap();
        assert_eq!(m.resets(), 1);
    }

    #[test]
    fn longer_writes_are_not_acknowledged() {
        let mut m = Ov2640::default();
        assert_eq!(m.transfer(&[0x44, 1, 2], &mut []), Err(AbiError::Transport));
        assert_eq!(m.writes(), 0);
        assert!(m.transfer(&[], &mut []).is_ok(), "an empty probe acks");
    }
}
