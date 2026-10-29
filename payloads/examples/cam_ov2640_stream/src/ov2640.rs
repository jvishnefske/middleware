//! OV2640 registers and the bring-up register table, as data.
//!
//! Register values derived from Zephyr's ov2640.c (Copyright (c) 2021
//! Antmicro, Apache-2.0) and the OV2640 datasheet. The names below are
//! Zephyr's (`drivers/video/ov2640.c`).
//!
//! The OV2640 has two register banks behind one SCCB address
//! ([`SCCB_ADDR`]): writing [`BANK_SEL`] (`0xFF`) selects the DSP bank
//! ([`BANK_SEL_DSP`]) or the sensor bank ([`BANK_SEL_SENSOR`]); every other
//! register address is interpreted in the selected bank.
//!
//! [`TABLE`] is everything written after the soft reset, in order
//! ([`TABLE_LEN`] = 219 writes), in four sections:
//!
//! 1. [`DEFAULT_REGS`] (173): Zephyr's `default_regs`, without the
//!    `{0x00, 0x00}` end marker that Zephyr's `ov2640_write_all` also
//!    writes (to DSP register 0, which the datasheet lists as reserved).
//! 2. [`JPEG_REGS`] (5): JPEG output, as Zephyr's
//!    `ov2640_set_output_format` and `ov2640_set_quality` do it, with the
//!    DSP held in reset while the mode changes: `RESET` = JPEG | DVP,
//!    `IMAGE_MODE` = [`IMAGE_MODE_JPEG_EN`], `QS` = [`QUALITY`], `RESET` = 0.
//! 3. [`OUTPUT_REGS`] (7): the first half of Zephyr's
//!    `ov2640_set_resolution`: DSP bypassed, the output size
//!    (`ZMOW` / `ZMOH` / `ZMHH`, from [`WINDOW`]) and `CLKRC` =
//!    [`CLKRC_ESP32`].
//! 4. [`WINDOW_REGS`] (34): Zephyr's `uxga_regs` (the sensor reads out
//!    its full 1600×1200 array), except that the DSP input window is
//!    the centred 1200×1200 square of [`WINDOW`] instead of the whole
//!    array, so the 240×240 output is not stretched. It ends with the DSP
//!    enabled and out of reset.
//!
//! # Departures from Zephyr for the ESP32
//!
//! The ESP32's I2S camera mode samples PCLK at no more than about
//! 10–20 MHz, and Zephyr's clocks (`CLKRC` 0x80, the frequency doubler;
//! `R_DVP_SP` automatic) run PCLK too fast for it: on the ESP32-CAM bytes
//! were lost. Every `CLKRC` write is therefore [`CLKRC_ESP32`] (0x00) and
//! every `R_DVP_SP` write [`R_DVP_SP_ESP32`] (0x08, a manual divider of 8),
//! the JPEG clock settings of esp32-camera's `ov2640.c` `set_window`
//! (Apache-2.0). With them the ESP32-CAM captures valid 240×240 JPEG
//! frames of 2–5 KB in about 85 ms.
//!
//! Zephyr's init then also sets AEC and AWB with read-modify-writes; both
//! are already on (`COM8` in [`DEFAULT_REGS`], `CTRL1`'s reset value), so
//! they are left out.

/// One register write: (address in the selected bank, value).
pub type RegWrite = (u8, u8);

/// The sensor's 7-bit SCCB (I2C) address.
pub const SCCB_ADDR: u8 = 0x30;

// Bank select, in both banks.

/// Bank select register.
pub const BANK_SEL: u8 = 0xFF;
/// [`BANK_SEL`] value: the DSP bank.
pub const BANK_SEL_DSP: u8 = 0x00;
/// [`BANK_SEL`] value: the sensor bank.
pub const BANK_SEL_SENSOR: u8 = 0x01;

// DSP bank (`BANK_SEL` = 0x00).

/// JPEG quantisation scale (DSP).
pub const QS: u8 = 0x44;
/// `CTRLI` (DSP): low-power DP and the H/V dividers.
pub const CTRLI: u8 = 0x50;
/// [`CTRLI`] bit: LP_DP.
pub const CTRLI_LP_DP: u8 = 0x80;
/// DSP input window width / 4, bits 7..0 (DSP).
pub const HSIZE: u8 = 0x51;
/// DSP input window height / 4, bits 7..0 (DSP).
pub const VSIZE: u8 = 0x52;
/// DSP input window x offset, bits 7..0 (DSP).
pub const XOFFL: u8 = 0x53;
/// DSP input window y offset, bits 7..0 (DSP).
pub const YOFFL: u8 = 0x54;
/// V_SIZE\[8\], OFFSET_Y\[10:8\], H_SIZE\[8\], OFFSET_X\[10:8\] (DSP).
pub const VHYX: u8 = 0x55;
/// H_SIZE\[9\] in bit 7 (DSP).
pub const TEST: u8 = 0x57;
/// Output width / 4, bits 7..0 (DSP).
pub const ZMOW: u8 = 0x5A;
/// Output height / 4, bits 7..0 (DSP).
pub const ZMOH: u8 = 0x5B;
/// OUTH\[8\] in bit 2, OUTW\[9:8\] in bits 1..0 (DSP).
pub const ZMHH: u8 = 0x5C;
/// SDE indirect address (DSP).
pub const BPADDR: u8 = 0x7C;
/// SDE indirect data (DSP).
pub const BPDATA: u8 = 0x7D;
/// `CTRL2` (DSP): DSP module enables.
pub const CTRL2: u8 = 0x86;
/// [`CTRL2`] bit: DCW.
pub const CTRL2_DCW_EN: u8 = 0x20;
/// [`CTRL2`] bit: SDE.
pub const CTRL2_SDE_EN: u8 = 0x10;
/// [`CTRL2`] bit: UV adjust.
pub const CTRL2_UV_ADJ_EN: u8 = 0x08;
/// [`CTRL2`] bit: UV average.
pub const CTRL2_UV_AVG_EN: u8 = 0x04;
/// [`CTRL2`] bit: colour matrix.
pub const CTRL2_CMX_EN: u8 = 0x01;
/// `CTRL3` (DSP).
pub const CTRL3: u8 = 0x87;
/// [`CTRL3`] bit: black pixel correction.
pub const CTRL3_BPC_EN: u8 = 0x80;
/// [`CTRL3`] bit: white pixel correction.
pub const CTRL3_WPC_EN: u8 = 0x40;
/// Sensor image size HSIZE\[11\], HSIZE\[2:0\], VSIZE\[2:0\] (DSP).
pub const SIZEL: u8 = 0x8C;
/// Sensor image width bits 10..3 (DSP).
pub const HSIZE8: u8 = 0xC0;
/// Sensor image height bits 10..3 (DSP).
pub const VSIZE8: u8 = 0xC1;
/// `CTRL0` (DSP): output format enables.
pub const CTRL0: u8 = 0xC2;
/// [`CTRL0`] bit: YUV422.
pub const CTRL0_YUV422: u8 = 0x08;
/// [`CTRL0`] bit: YUV.
pub const CTRL0_YUV_EN: u8 = 0x04;
/// [`CTRL0`] bit: RGB.
pub const CTRL0_RGB_EN: u8 = 0x02;
/// DVP output speed (DSP).
pub const R_DVP_SP: u8 = 0xD3;
/// [`R_DVP_SP`] bit: automatic PCLK divider.
pub const R_DVP_SP_AUTO_MODE: u8 = 0x80;
/// DSP bypass (DSP).
pub const R_BYPASS: u8 = 0x05;
/// [`R_BYPASS`] value: DSP enabled.
pub const R_BYPASS_DSP_EN: u8 = 0x00;
/// [`R_BYPASS`] value: DSP bypassed (while its window changes).
pub const R_BYPASS_DSP_BYPAS: u8 = 0x01;
/// Output image mode (DSP).
pub const IMAGE_MODE: u8 = 0xDA;
/// [`IMAGE_MODE`] value: JPEG output.
pub const IMAGE_MODE_JPEG_EN: u8 = 0x10;
/// Module resets (DSP).
pub const RESET: u8 = 0xE0;
/// [`RESET`] bit: the JPEG encoder.
pub const RESET_JPEG: u8 = 0x10;
/// [`RESET`] bit: the DVP port.
pub const RESET_DVP: u8 = 0x04;
/// Microcontroller BIST control (DSP).
pub const MC_BIST: u8 = 0xF9;
/// [`MC_BIST`] bit: microcontroller reset.
pub const MC_BIST_RESET: u8 = 0x80;
/// [`MC_BIST`] bit: boot ROM select.
pub const MC_BIST_BOOT_ROM_SEL: u8 = 0x40;

// Sensor bank (`BANK_SEL` = 0x01).

/// `COM1` (sensor): vertical window LSBs and dummy frames.
pub const COM1: u8 = 0x03;
/// `REG04` (sensor): mirror / flip.
pub const REG04: u8 = 0x04;
/// [`REG04`] base value.
pub const REG04_DEFAULT: u8 = 0x28;
/// [`REG04`] bit: HREF.
pub const REG04_HREF_EN: u8 = 0x08;
/// `COM2` (sensor): output drive.
pub const COM2: u8 = 0x09;
/// [`COM2`] value: output drive 3×.
pub const COM2_OUT_DRIVE_3X: u8 = 0x02;
/// Product ID, high byte (sensor).
pub const REG_PID: u8 = 0x0A;
/// Product ID, low byte: the version (sensor).
pub const REG_VER: u8 = 0x0B;
/// `COM3` (sensor): banding.
pub const COM3: u8 = 0x0C;
/// [`COM3`] base value.
pub const COM3_DEFAULT: u8 = 0x38;
/// [`COM3`] bit: automatic banding.
pub const COM3_BAND_AUTO: u8 = 0x02;
/// Clock divider (sensor).
pub const CLKRC: u8 = 0x11;
/// `COM7` (sensor): soft reset and resolution.
pub const COM7: u8 = 0x12;
/// [`COM7`] bit: soft reset (self-clearing; every register back to its
/// default).
pub const COM7_SRST: u8 = 0x80;
/// [`COM7`] value: UXGA (1600×1200) readout.
pub const COM7_RES_UXGA: u8 = 0x00;
/// `COM8` (sensor): AEC / AGC / banding filter.
pub const COM8: u8 = 0x13;
/// [`COM8`] base value.
pub const COM8_DEFAULT: u8 = 0xC0;
/// [`COM8`] bit: banding filter.
pub const COM8_BNDF_EN: u8 = 0x20;
/// [`COM8`] bit: automatic gain.
pub const COM8_AGC_EN: u8 = 0x04;
/// [`COM8`] bit: automatic exposure.
pub const COM8_AEC_EN: u8 = 0x01;
/// `COM9` (sensor): AGC gain ceiling.
pub const COM9: u8 = 0x14;
/// [`COM9`] base value.
pub const COM9_DEFAULT: u8 = 0x08;
/// [`COM9`] gain ceiling: 8×.
pub const COM9_AGC_GAIN_8X: u8 = 0x02;
/// `COM10` (sensor): sync polarities.
pub const COM10: u8 = 0x15;
/// Horizontal window start (sensor).
pub const HSTART: u8 = 0x17;
/// Horizontal window end (sensor).
pub const HSTOP: u8 = 0x18;
/// Vertical window start (sensor).
pub const VSTART: u8 = 0x19;
/// Vertical window end (sensor).
pub const VSTOP: u8 = 0x1A;
/// AGC/AEC upper limit (sensor).
pub const AEW: u8 = 0x24;
/// AGC/AEC lower limit (sensor).
pub const AEB: u8 = 0x25;
/// AGC/AEC fast-mode region (sensor).
pub const VV: u8 = 0x26;
/// `REG32` (sensor): horizontal window LSBs, PCLK divider.
pub const REG32: u8 = 0x32;
/// [`REG32`] value for UXGA.
pub const REG32_UXGA: u8 = 0x36;
/// `ARCOM2` (sensor).
pub const ARCOM2: u8 = 0x34;
/// Frame length adjustment, low byte (sensor).
pub const FLL: u8 = 0x46;
/// Frame length adjustment, high byte (sensor).
pub const FLH: u8 = 0x47;
/// `COM19` (sensor): zoom LSBs.
pub const COM19: u8 = 0x48;
/// Zoom MSBs (sensor).
pub const ZOOMS: u8 = 0x49;
/// 50 Hz banding AEC (sensor).
pub const BD50: u8 = 0x4F;
/// 60 Hz banding AEC (sensor).
pub const BD60: u8 = 0x50;
/// AVGsel (sensor).
pub const REG5D: u8 = 0x5D;
/// AVGsel (sensor).
pub const REG5E: u8 = 0x5E;
/// AVGsel (sensor).
pub const REG5F: u8 = 0x5F;
/// AVGsel (sensor).
pub const REG60: u8 = 0x60;
/// Histogram low level (sensor).
pub const HISTO_LOW: u8 = 0x61;
/// Histogram high level (sensor).
pub const HISTO_HIGH: u8 = 0x62;

/// Expected [`REG_PID`].
pub const PID_OV2640: u8 = 0x26;
/// [`REG_VER`] values accepted: 0x42 (what Zephyr checks for) and 0x41,
/// an earlier revision of the same part.
pub const VER_OV2640: [u8; 2] = [0x41, 0x42];

/// Whether `(pid, ver)` identify an OV2640.
pub const fn is_ov2640(pid: u8, ver: u8) -> bool {
    pid == PID_OV2640 && (ver == VER_OV2640[0] || ver == VER_OV2640[1])
}

/// Zephyr's `REG04_SET(x)`.
pub const fn reg04_set(x: u8) -> u8 {
    REG04_DEFAULT | x
}

/// Zephyr's `COM3_BAND_SET(x)`.
pub const fn com3_band_set(x: u8) -> u8 {
    COM3_DEFAULT | x
}

/// Zephyr's `COM8_SET(x)`.
pub const fn com8_set(x: u8) -> u8 {
    COM8_DEFAULT | x
}

/// Zephyr's `COM9_AGC_SET(x)`.
pub const fn com9_agc_set(x: u8) -> u8 {
    COM9_DEFAULT | (x << 5)
}

/// Zephyr's `VV_AGC_TH_SET(h, l)`.
pub const fn vv_agc_th_set(h: u8, l: u8) -> u8 {
    (h << 4) | (l & 0x0F)
}

/// JPEG quantisation scale written to [`QS`]: lower is better quality and
/// larger frames. It must keep every 240×240 frame under the badge
/// viewer's 8 KiB frame slots (and the firmware's capture buffer). 12 was
/// the first estimate; on the ESP32-CAM some scenes then gave frames over
/// 8 KB (the firmware reported an overflow), so it was raised to 15.
/// Scenes with more detail make larger frames: raise it further if frames
/// still overflow.
pub const QUALITY: u8 = 15;

/// `CLKRC` for the ESP32: 0x00, no frequency doubler and no divider
/// (Zephyr writes 0x80, the doubler). The ESP32's I2S camera mode samples
/// PCLK at no more than about 10–20 MHz; this is esp32-camera's JPEG
/// setting (`ov2640.c` `set_window`, Apache-2.0). See the module docs.
pub const CLKRC_ESP32: u8 = 0x00;

/// `R_DVP_SP` for the ESP32: 0x08, a manual PCLK divider of 8 without
/// [`R_DVP_SP_AUTO_MODE`] (Zephyr writes automatic mode). Like
/// [`CLKRC_ESP32`], esp32-camera's JPEG setting, slow enough for the
/// ESP32's I2S camera receiver.
pub const R_DVP_SP_ESP32: u8 = 0x08;

/// The sensor's full readout (UXGA), what `uxga_regs` sets.
pub const SENSOR_W: u16 = 1600;
/// See [`SENSOR_W`].
pub const SENSOR_H: u16 = 1200;

/// The DSP input window (a rectangle of the sensor's readout) and the
/// output size it is scaled to, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Left edge in the readout.
    pub x: u16,
    /// Top edge in the readout.
    pub y: u16,
    /// Window width.
    pub width: u16,
    /// Window height.
    pub height: u16,
    /// Output (JPEG) width.
    pub out_width: u16,
    /// Output (JPEG) height.
    pub out_height: u16,
}

/// Output width and height: the badge panel's 240×240.
pub const OUT_SIZE: u16 = 240;

/// The window used: the centred [`SENSOR_H`]-square of the readout
/// (x 200, y 0, 1200×1200), scaled 5:1 to [`OUT_SIZE`]×[`OUT_SIZE`].
pub const WINDOW: Window = Window {
    x: (SENSOR_W - SENSOR_H) / 2,
    y: 0,
    width: SENSOR_H,
    height: SENSOR_H,
    out_width: OUT_SIZE,
    out_height: OUT_SIZE,
};

impl Window {
    /// Whether the registers can express it: inside the readout, every
    /// size a multiple of 4 (the registers hold size / 4), sizes / 4 within
    /// their 10 (window) and 10 / 9 (output) bits, offsets within 11 bits,
    /// and an output no larger than the window (the DSP only scales down).
    pub const fn is_valid(&self) -> bool {
        let in_readout = self.x as u32 + self.width as u32 <= SENSOR_W as u32
            && self.y as u32 + self.height as u32 <= SENSOR_H as u32;
        let by4 = self.width.is_multiple_of(4)
            && self.height.is_multiple_of(4)
            && self.out_width.is_multiple_of(4)
            && self.out_height.is_multiple_of(4);
        let nonzero = self.out_width > 0 && self.out_height > 0;
        let fits = self.width / 4 < 1024
            && self.height / 4 < 512
            && self.out_width / 4 < 1024
            && self.out_height / 4 < 512
            && self.x < 2048
            && self.y < 2048;
        let shrinks = self.out_width <= self.width && self.out_height <= self.height;
        in_readout && by4 && nonzero && fits && shrinks
    }

    /// The window's register values.
    pub const fn regs(&self) -> WindowRegs {
        let w4 = self.width / 4;
        let h4 = self.height / 4;
        let ow4 = self.out_width / 4;
        let oh4 = self.out_height / 4;
        WindowRegs {
            hsize: (w4 & 0xFF) as u8,
            vsize: (h4 & 0xFF) as u8,
            xoffl: (self.x & 0xFF) as u8,
            yoffl: (self.y & 0xFF) as u8,
            vhyx: (((h4 >> 1) & 0x80)
                | ((self.y >> 4) & 0x70)
                | ((w4 >> 5) & 0x08)
                | ((self.x >> 8) & 0x07)) as u8,
            test: ((w4 >> 2) & 0x80) as u8,
            zmow: (ow4 & 0xFF) as u8,
            zmoh: (oh4 & 0xFF) as u8,
            zmhh: (((oh4 >> 6) & 0x04) | ((ow4 >> 8) & 0x03)) as u8,
        }
    }
}

/// Register values of a [`Window`] ([`Window::regs`]), named after their
/// registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRegs {
    /// [`HSIZE`]: width / 4, bits 7..0.
    pub hsize: u8,
    /// [`VSIZE`]: height / 4, bits 7..0.
    pub vsize: u8,
    /// [`XOFFL`]: x, bits 7..0.
    pub xoffl: u8,
    /// [`YOFFL`]: y, bits 7..0.
    pub yoffl: u8,
    /// [`VHYX`]: (height / 4)\[8\] in bit 7, y\[10:8\] in 6..4, (width /
    /// 4)\[8\] in bit 3, x\[10:8\] in 2..0.
    pub vhyx: u8,
    /// [`TEST`]: (width / 4)\[9\] in bit 7.
    pub test: u8,
    /// [`ZMOW`]: output width / 4, bits 7..0.
    pub zmow: u8,
    /// [`ZMOH`]: output height / 4, bits 7..0.
    pub zmoh: u8,
    /// [`ZMHH`]: (output height / 4)\[8\] in bit 2, (output width / 4)\[9:8\]
    /// in bits 1..0.
    pub zmhh: u8,
}

const _: () = assert!(WINDOW.is_valid());

/// [`WINDOW`]'s register values.
pub const WIN: WindowRegs = WINDOW.regs();

/// [`SIZEL`] for a `w`×`h` readout: w\[11\] in bit 6, w\[2:0\] in 5..3,
/// h\[2:0\] in 2..0 (Zephyr's expression).
pub const fn sizel(w: u16, h: u16) -> u8 {
    (((w >> 6) & 0x40) | ((w & 0x7) << 3) | (h & 0x7)) as u8
}

/// Section 1 of [`TABLE`]: Zephyr's `default_regs` without its end marker.
pub const DEFAULT_REGS: [RegWrite; 173] = [
    (BANK_SEL, BANK_SEL_DSP),
    (0x2C, 0xFF),
    (0x2E, 0xDF),
    (BANK_SEL, BANK_SEL_SENSOR),
    (0x3C, 0x32),
    (CLKRC, CLKRC_ESP32),      // Zephyr: 0x80 (doubler); see CLKRC_ESP32
    (COM2, COM2_OUT_DRIVE_3X), // Output drive x2
    (REG04, reg04_set(REG04_HREF_EN)),
    (COM8, com8_set(COM8_BNDF_EN | COM8_AGC_EN | COM8_AEC_EN)),
    (COM9, com9_agc_set(COM9_AGC_GAIN_8X)),
    (COM10, 0x00), // Invert VSYNC
    (0x2C, 0x0C),
    (0x33, 0x78),
    (0x3A, 0x33),
    (0x3B, 0xFB),
    (0x3E, 0x00),
    (0x43, 0x11),
    (0x16, 0x10),
    (0x39, 0x02),
    (0x35, 0x88),
    (0x22, 0x0A),
    (0x37, 0x40),
    (0x23, 0x00),
    (ARCOM2, 0xA0),
    (0x06, 0x02),
    (0x06, 0x88),
    (0x07, 0xC0),
    (0x0D, 0xB7),
    (0x0E, 0x01),
    (0x4C, 0x00),
    (0x4A, 0x81),
    (0x21, 0x99),
    (AEW, 0x40),
    (AEB, 0x38),
    // AGC/AEC fast mode operating region
    (VV, vv_agc_th_set(0x08, 0x02)),
    (COM19, 0x00), // Zoom control 2 LSBs
    (ZOOMS, 0x00), // Zoom control 8 MSBs
    (0x5C, 0x00),
    (0x63, 0x00),
    (FLL, 0x00),
    (FLH, 0x00),
    // Set banding filter
    (COM3, com3_band_set(COM3_BAND_AUTO)),
    (REG5D, 0x55),
    (REG5E, 0x7D),
    (REG5F, 0x7D),
    (REG60, 0x55),
    (HISTO_LOW, 0x70),
    (HISTO_HIGH, 0x80),
    (0x7C, 0x05),
    (0x20, 0x80),
    (0x28, 0x30),
    (0x6C, 0x00),
    (0x6D, 0x80),
    (0x6E, 0x00),
    (0x70, 0x02),
    (0x71, 0x94),
    (0x73, 0xC1),
    (0x3D, 0x34),
    (0x5A, 0x57),
    (BD50, 0xBB),
    (BD60, 0x9C),
    (BANK_SEL, BANK_SEL_DSP),
    (0xE5, 0x7F),
    (MC_BIST, MC_BIST_RESET | MC_BIST_BOOT_ROM_SEL),
    (0x41, 0x24),
    (RESET, RESET_JPEG | RESET_DVP),
    (0x76, 0xFF),
    (0x33, 0xA0),
    (0x42, 0x20),
    (0x43, 0x18),
    (0x4C, 0x00),
    (CTRL3, CTRL3_BPC_EN | CTRL3_WPC_EN | 0x10),
    (0x88, 0x3F),
    (0xD7, 0x03),
    (0xD9, 0x10),
    // Zephyr: R_DVP_SP_AUTO_MODE | 0x2; see R_DVP_SP_ESP32.
    (R_DVP_SP, R_DVP_SP_ESP32),
    (0xC8, 0x08),
    (0xC9, 0x80),
    (BPADDR, 0x00),
    (BPDATA, 0x00),
    (BPADDR, 0x03),
    (BPDATA, 0x48),
    (BPDATA, 0x48),
    (BPADDR, 0x08),
    (BPDATA, 0x20),
    (BPDATA, 0x10),
    (BPDATA, 0x0E),
    (0x90, 0x00),
    (0x91, 0x0E),
    (0x91, 0x1A),
    (0x91, 0x31),
    (0x91, 0x5A),
    (0x91, 0x69),
    (0x91, 0x75),
    (0x91, 0x7E),
    (0x91, 0x88),
    (0x91, 0x8F),
    (0x91, 0x96),
    (0x91, 0xA3),
    (0x91, 0xAF),
    (0x91, 0xC4),
    (0x91, 0xD7),
    (0x91, 0xE8),
    (0x91, 0x20),
    (0x92, 0x00),
    (0x93, 0x06),
    (0x93, 0xE3),
    (0x93, 0x03),
    (0x93, 0x03),
    (0x93, 0x00),
    (0x93, 0x02),
    (0x93, 0x00),
    (0x93, 0x00),
    (0x93, 0x00),
    (0x93, 0x00),
    (0x93, 0x00),
    (0x93, 0x00),
    (0x93, 0x00),
    (0x96, 0x00),
    (0x97, 0x08),
    (0x97, 0x19),
    (0x97, 0x02),
    (0x97, 0x0C),
    (0x97, 0x24),
    (0x97, 0x30),
    (0x97, 0x28),
    (0x97, 0x26),
    (0x97, 0x02),
    (0x97, 0x98),
    (0x97, 0x80),
    (0x97, 0x00),
    (0x97, 0x00),
    (0xA4, 0x00),
    (0xA8, 0x00),
    (0xC5, 0x11),
    (0xC6, 0x51),
    (0xBF, 0x80),
    (0xC7, 0x10),
    (0xB6, 0x66),
    (0xB8, 0xA5),
    (0xB7, 0x64),
    (0xB9, 0x7C),
    (0xB3, 0xAF),
    (0xB4, 0x97),
    (0xB5, 0xFF),
    (0xB0, 0xC5),
    (0xB1, 0x94),
    (0xB2, 0x0F),
    (0xC4, 0x5C),
    (0xA6, 0x00),
    (0xA7, 0x20),
    (0xA7, 0xD8),
    (0xA7, 0x1B),
    (0xA7, 0x31),
    (0xA7, 0x00),
    (0xA7, 0x18),
    (0xA7, 0x20),
    (0xA7, 0xD8),
    (0xA7, 0x19),
    (0xA7, 0x31),
    (0xA7, 0x00),
    (0xA7, 0x18),
    (0xA7, 0x20),
    (0xA7, 0xD8),
    (0xA7, 0x19),
    (0xA7, 0x31),
    (0xA7, 0x00),
    (0xA7, 0x18),
    (0x7F, 0x00),
    (0xE5, 0x1F),
    (0xE1, 0x77),
    (0xDD, 0x7F),
    (CTRL0, CTRL0_YUV422 | CTRL0_YUV_EN | CTRL0_RGB_EN),
];

/// Section 2 of [`TABLE`]: JPEG output at [`QUALITY`], with the JPEG
/// encoder and DVP port held in reset while the mode changes.
pub const JPEG_REGS: [RegWrite; 5] = [
    (BANK_SEL, BANK_SEL_DSP),
    (RESET, RESET_JPEG | RESET_DVP),
    (IMAGE_MODE, IMAGE_MODE_JPEG_EN),
    (QS, QUALITY),
    (RESET, 0x00),
];

/// Section 3 of [`TABLE`]: DSP bypassed, the output size of [`WINDOW`],
/// and the sensor clock ([`CLKRC_ESP32`]; Zephyr writes its
/// `clock-rate-control`, 0x80 by default).
pub const OUTPUT_REGS: [RegWrite; 7] = [
    (BANK_SEL, BANK_SEL_DSP),
    (R_BYPASS, R_BYPASS_DSP_BYPAS),
    (ZMOW, WIN.zmow),
    (ZMOH, WIN.zmoh),
    (ZMHH, WIN.zmhh),
    (BANK_SEL, BANK_SEL_SENSOR),
    (CLKRC, CLKRC_ESP32),
];

/// Section 4 of [`TABLE`]: Zephyr's `uxga_regs` (UXGA readout) with the
/// DSP input window set to [`WINDOW`], ending with the DSP enabled and the
/// DVP out of reset.
pub const WINDOW_REGS: [RegWrite; 34] = [
    (BANK_SEL, BANK_SEL_SENSOR),
    // DSP input image resolution and window size control.
    (COM7, COM7_RES_UXGA),
    (COM1, 0x0F),        // UXGA=0x0F, SVGA=0x0A, CIF=0x06
    (REG32, REG32_UXGA), // UXGA=0x36, SVGA/CIF=0x09
    (HSTART, 0x11),      // UXGA=0x11, SVGA/CIF=0x11
    (HSTOP, 0x75),       // UXGA=0x75, SVGA/CIF=0x43
    (VSTART, 0x01),      // UXGA=0x01, SVGA/CIF=0x00
    (VSTOP, 0x97),       // UXGA=0x97, SVGA/CIF=0x4b
    (0x3D, 0x34),        // UXGA=0x34, SVGA/CIF=0x38
    (0x35, 0x88),
    (0x22, 0x0A),
    (0x37, 0x40),
    (0x34, 0xA0),
    (0x06, 0x02),
    (0x0D, 0xB7),
    (0x0E, 0x01),
    (0x42, 0x83),
    // DSP input image size and offset; the output is scaled to
    // ZMOW / ZMOH (OUTPUT_REGS).
    (BANK_SEL, BANK_SEL_DSP),
    (R_BYPASS, R_BYPASS_DSP_BYPAS),
    (RESET, RESET_DVP),
    (HSIZE8, (SENSOR_W >> 3) as u8), // HSIZE[10:3] of the readout
    (VSIZE8, (SENSOR_H >> 3) as u8), // VSIZE[10:3] of the readout
    (SIZEL, sizel(SENSOR_W, SENSOR_H)),
    (XOFFL, WIN.xoffl),
    (YOFFL, WIN.yoffl),
    (HSIZE, WIN.hsize),
    (VSIZE, WIN.vsize),
    (VHYX, WIN.vhyx),
    (TEST, WIN.test),
    (
        CTRL2,
        CTRL2_DCW_EN | CTRL2_SDE_EN | CTRL2_UV_AVG_EN | CTRL2_CMX_EN | CTRL2_UV_ADJ_EN,
    ),
    // H_DIVIDER / V_DIVIDER 0: the zoom does all the scaling.
    (CTRLI, CTRLI_LP_DP),
    // DVP PCLK divider: Zephyr's R_DVP_SP_AUTO_MODE | 0x04 is too fast
    // for the ESP32; see R_DVP_SP_ESP32.
    (R_DVP_SP, R_DVP_SP_ESP32),
    (R_BYPASS, R_BYPASS_DSP_EN),
    (RESET, 0x00),
];

/// Number of writes in [`TABLE`].
pub const TABLE_LEN: usize =
    DEFAULT_REGS.len() + JPEG_REGS.len() + OUTPUT_REGS.len() + WINDOW_REGS.len();

/// The four sections in one array, in the order they are written.
pub const TABLE: [RegWrite; TABLE_LEN] =
    concat(&[&DEFAULT_REGS, &JPEG_REGS, &OUTPUT_REGS, &WINDOW_REGS]);

/// `parts` one after another; evaluated at compile time only (a length
/// mismatch is a build error).
const fn concat<const N: usize>(parts: &[&[RegWrite]]) -> [RegWrite; N] {
    let mut out = [(0u8, 0u8); N];
    let mut n = 0;
    let mut p = 0;
    while p < parts.len() {
        let part = parts[p];
        let mut i = 0;
        while i < part.len() {
            out[n] = part[i];
            n += 1;
            i += 1;
        }
        p += 1;
    }
    assert!(n == N);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_is_the_centred_1200_square_scaled_to_240() {
        assert_eq!(
            WINDOW,
            Window {
                x: 200,
                y: 0,
                width: 1200,
                height: 1200,
                out_width: 240,
                out_height: 240,
            }
        );
        assert!(WINDOW.is_valid());
    }

    #[test]
    fn window_registers_for_240x240() {
        // 1200 / 4 = 300 = 0x12C: low byte 0x2C, bit 8 in VHYX, bit 9 (0)
        // in TEST; x 200 = 0xC8; output 240 / 4 = 60, no high bits.
        assert_eq!(
            WIN,
            WindowRegs {
                hsize: 0x2C,
                vsize: 0x2C,
                xoffl: 0xC8,
                yoffl: 0x00,
                vhyx: 0x88,
                test: 0x00,
                zmow: 60,
                zmoh: 60,
                zmhh: 0,
            }
        );
    }

    #[test]
    fn window_registers_match_zephyrs_uxga_expressions() {
        // Zephyr's uxga_regs for the whole 1600×1200 array, and its
        // set_resolution expressions for a 1600×1200 output.
        let full = Window {
            x: 0,
            y: 0,
            width: 1600,
            height: 1200,
            out_width: 1600,
            out_height: 1200,
        };
        let r = full.regs();
        assert_eq!(r.hsize, ((1600u16 >> 2) & 0xFF) as u8);
        assert_eq!(r.vsize, ((1200u16 >> 2) & 0xFF) as u8);
        assert_eq!(
            r.vhyx,
            (((1200u16 >> 3) & 0x80) | ((1600u16 >> 7) & 0x08)) as u8
        );
        assert_eq!(r.test, ((1600u16 >> 4) & 0x80) as u8);
        assert_eq!(r.zmow, ((1600u16 >> 2) & 0xFF) as u8);
        assert_eq!(r.zmoh, ((1200u16 >> 2) & 0xFF) as u8);
        assert_eq!(
            r.zmhh,
            (((1200u16 >> 8) & 0x04) | ((1600u16 >> 10) & 0x03)) as u8
        );
        assert_eq!(sizel(1600, 1200), 0);
        // High offset and size bits land where the datasheet puts them.
        let odd = Window {
            x: 0x5AB,
            y: 0x123,
            width: 2044,
            height: 1024,
            out_width: 4,
            out_height: 4,
        };
        let r = odd.regs();
        assert_eq!(r.xoffl, 0xAB);
        assert_eq!(r.yoffl, 0x23);
        // h/4 = 256: bit 8 -> 0x80; y[10:8] = 1 -> 0x10; w/4 = 511: bit 8
        // -> 0x08; x[10:8] = 5.
        assert_eq!(r.vhyx, 0x80 | 0x10 | 0x08 | 0x05);
        assert_eq!(r.test, 0x00);
        let wide = Window { width: 2048, ..odd };
        assert_eq!(wide.regs().test, 0x80, "w/4 = 512: bit 9");
        let big_out = Window {
            out_width: 1024 * 4 - 4,
            out_height: 256 * 4,
            ..full
        };
        assert_eq!(big_out.regs().zmhh, 0x04 | 0x03);
    }

    #[test]
    fn invalid_windows_are_refused() {
        let w = WINDOW;
        assert!(!Window { x: 401, ..w }.is_valid(), "past the readout");
        assert!(!Window { width: 1202, ..w }.is_valid(), "not by 4");
        assert!(
            !Window {
                out_width: 242,
                ..w
            }
            .is_valid(),
            "not by 4"
        );
        assert!(!Window { out_height: 0, ..w }.is_valid(), "empty");
        assert!(
            !Window {
                out_width: 1204,
                ..w
            }
            .is_valid(),
            "scales up"
        );
    }

    #[test]
    fn identity_check() {
        assert!(is_ov2640(0x26, 0x41));
        assert!(is_ov2640(0x26, 0x42));
        assert!(!is_ov2640(0x26, 0x43));
        assert!(!is_ov2640(0x77, 0x42));
        assert!(!is_ov2640(0xFF, 0xFF), "a floating bus");
    }

    #[test]
    fn table_is_the_four_sections_in_order() {
        assert_eq!(TABLE_LEN, 219);
        let mut all = Vec::new();
        for part in [&DEFAULT_REGS[..], &JPEG_REGS, &OUTPUT_REGS, &WINDOW_REGS] {
            all.extend_from_slice(part);
        }
        assert_eq!(&TABLE[..], &all[..]);
    }

    #[test]
    fn every_pclk_setting_is_the_esp32_one() {
        // Zephyr's CLKRC 0x80 and automatic R_DVP_SP are replaced
        // everywhere: the last write of each, which stays in effect, too.
        assert_eq!((CLKRC_ESP32, R_DVP_SP_ESP32), (0x00, 0x08));
        let in_bank = |reg: u8| {
            let mut bank = BANK_SEL_DSP;
            TABLE
                .iter()
                .filter_map(move |&(r, v)| {
                    if r == BANK_SEL {
                        bank = v;
                        None
                    } else {
                        (r == reg).then_some((bank, v))
                    }
                })
                .collect::<Vec<_>>()
        };
        let clkrc = in_bank(CLKRC);
        let sensor_clkrc: Vec<_> = clkrc.iter().filter(|w| w.0 == BANK_SEL_SENSOR).collect();
        assert_eq!(sensor_clkrc.len(), 2, "default table and OUTPUT_REGS");
        assert!(sensor_clkrc.iter().all(|w| w.1 == CLKRC_ESP32));
        let dvp = in_bank(R_DVP_SP);
        assert_eq!(dvp.len(), 2, "default table and WINDOW_REGS");
        assert!(dvp.iter().all(|&w| w == (BANK_SEL_DSP, R_DVP_SP_ESP32)));
        assert_eq!(QUALITY, 15);
    }

    #[test]
    fn zephyr_macros() {
        assert_eq!(reg04_set(REG04_HREF_EN), 0x28 | 0x08);
        assert_eq!(com8_set(COM8_BNDF_EN | COM8_AGC_EN | COM8_AEC_EN), 0xE5);
        assert_eq!(com9_agc_set(COM9_AGC_GAIN_8X), 0x48);
        assert_eq!(vv_agc_th_set(0x08, 0x02), 0x82);
        assert_eq!(com3_band_set(COM3_BAND_AUTO), 0x3A);
    }

    #[test]
    fn default_section_spot_checks() {
        // First, last and a few distinctive entries of Zephyr's table.
        assert_eq!(DEFAULT_REGS[0], (0xFF, 0x00));
        assert_eq!(DEFAULT_REGS[5], (0x11, 0x00), "CLKRC_ESP32");
        assert_eq!(DEFAULT_REGS[DEFAULT_REGS.len() - 1], (0xC2, 0x0E));
        assert!(DEFAULT_REGS.contains(&(0xF9, 0xC0)));
        assert!(DEFAULT_REGS.contains(&(0xE0, 0x14)));
        assert!(!DEFAULT_REGS.contains(&(0x00, 0x00)), "no end marker");
    }
}
