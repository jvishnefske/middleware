//! Blocking console on LPUART1 (GPIO_AD_B0_12 TX / _13 RX, ALT2), the
//! EVKB's OpenSDA virtual COM port, 115200 8N1.
//!
//! The UART clock is the 24 MHz oscillator undivided (CSCDR1 UART_CLK_SEL
//! = OSC, PODF = 1), independent of the boot clocks; baud = 24 MHz /
//! (16 × 13) = 115 385 (0.16 % off). Writes poll TDRE with a bound, so a
//! stuck transmitter drops output instead of hanging the caller.

use embassy_nxp::pac;

/// Polls of TDRE per byte before the byte is dropped (about 10 ms at the
/// slowest core clock).
const TX_POLLS: u32 = 1_000_000;

/// LPUART1 as a `core::fmt::Write` sink.
pub struct Console {
    _uart: embassy_nxp::Peri<'static, embassy_nxp::peripherals::LPUART1>,
}

impl Console {
    /// Clocks, pads and the transmitter/receiver at 115200 8N1.
    pub fn new(uart: embassy_nxp::Peri<'static, embassy_nxp::peripherals::LPUART1>) -> Console {
        pac::CCM.cscdr1().modify(|w| {
            w.set_uart_clk_sel(pac::ccm::vals::UartClkSel::UartClkSel1);
            w.set_uart_clk_podf(pac::ccm::vals::UartClkPodf::Divide1);
        });
        pac::CCM.ccgr5().modify(|w| w.set_cg12(0b11));
        pac::IOMUXC
            .sw_mux_ctl_pad_gpio_ad_b0_12()
            .write(|w| w.set_mux_mode(2));
        pac::IOMUXC
            .sw_mux_ctl_pad_gpio_ad_b0_13()
            .write(|w| w.set_mux_mode(2));
        let u = pac::LPUART1;
        u.global().write(|w| w.set_rst(true));
        u.global().write(|w| w.set_rst(false));
        u.baud().write(|w| {
            w.set_osr(pac::lpuart::vals::Osr::from_bits(15));
            w.set_sbr(13);
        });
        u.ctrl().write(|w| {
            w.set_te(true);
            w.set_re(true);
        });
        Console { _uart: uart }
    }

    /// Writes `bytes`, each after the transmit buffer has room (bounded).
    pub fn put(&mut self, bytes: &[u8]) {
        for &b in bytes {
            for _ in 0..TX_POLLS {
                if pac::LPUART1.stat().read().tdre() != pac::lpuart::vals::Tdre::Txdata {
                    break;
                }
            }
            pac::LPUART1
                .data()
                .write_value(pac::lpuart::regs::Data(u32::from(b)));
        }
    }
}

impl Console {
    /// Waits (bounded) until the last byte has left the transmitter
    /// (STAT.TC), so a reset or a jump into an image that re-initialises
    /// LPUART1 does not cut the output short.
    pub fn flush(&mut self) {
        for _ in 0..TX_POLLS {
            if pac::LPUART1.stat().read().tc() != pac::lpuart::vals::Tc::Active {
                break;
            }
        }
    }
}

impl core::fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.put(s.as_bytes());
        Ok(())
    }
}
