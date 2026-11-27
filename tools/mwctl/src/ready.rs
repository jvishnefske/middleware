//! `mwctl probe`: is the board on this port ready to **update** (a base
//! firmware answers, so a payload can be written) or ready to **flash**
//! (no firmware answers, and this is how its bootloader is reached)?
//!
//! Development boards with a USB serial port fall into a few families,
//! told apart by the port's USB id ([`family`]; the table is the one
//! `tests/hil/boards.py` uses, plus the generic bootloader conventions):
//!
//! | family | flashing |
//! |---|---|
//! | Espressif USB-Serial-JTAG, UART bridges (CP210x, CH340, CH9102, FT232R) | directly over the same serial port: the ROM bootloader, entered by the client through DTR/RTS |
//! | rp-base (Pico, Pico 2), the RT1050's ports | the running firmware's A/B updater (`mwctl fw update`); a Pico first takes a UF2 in BOOTSEL mode, where it has no serial port |
//! | micro:bit DAPLink VCOM | the repository's nRF52 bootloader (`mwctl reflash`), entered by hand |
//! | Arduino and Adafruit native-USB boards | a bootloader the client triggers by opening the port at 1200 baud; no base firmware exists for them yet |
//!
//! [`probe`] asks, in order: `sys/info` (a base firmware answers: update);
//! then, on a family that can hold an Espressif chip, one ROM `SYNC`
//! command ([`esp_sync`]; a chip already in download mode answers: flash
//! now); otherwise the family alone decides. It never resets the board and
//! never changes its baud rate, so probing a running board is harmless: the
//! `SYNC` bytes are noise to the serial protocol's splitter.
//!
//! The report ([`Report::lines`]) is `name: value` lines; the first is
//! always `ready: update`, `ready: flash` or `ready: unknown`, which is
//! what the web page reads.

use crate::device::{Device, Info};
use crate::error::Error;
use crate::link::{Link, Timeouts, Transport};
use std::time::{Duration, Instant};

/// A USB vendor and product id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbId {
    /// Vendor id.
    pub vid: u16,
    /// Product id.
    pub pid: u16,
}

impl UsbId {
    /// Parses `VVVV:PPPP` (hex, as `lsusb` prints it).
    pub fn parse(s: &str) -> Option<Self> {
        let (v, p) = s.split_once(':')?;
        let hex = |t: &str| {
            ((1..=4).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| u16::from_str_radix(t, 16).ok())
                .flatten()
        };
        Some(UsbId { vid: hex(v)?, pid: hex(p)? })
    }
}

impl std::fmt::Display for UsbId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vid, self.pid)
    }
}

/// What a port's USB id says about the board behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// An ESP32-S3 or ESP32-C3 on its native USB-Serial-JTAG port.
    EspUsbJtag,
    /// A USB-UART bridge chip, as on ESP32 and ESP8266 development boards.
    UartBridge,
    /// The RP base firmware's own port (Pico, Pico 2).
    RpBase,
    /// The RT1050-EVKB (its debug probe's VCOM or the firmware's USB port).
    Rt1050,
    /// The micro:bit v2's DAPLink VCOM.
    Microbit,
    /// A board whose bootloader the client starts by opening the port at
    /// 1200 baud (Arduino and Adafruit native-USB boards).
    Touch1200,
    /// Not in the table, or no id was given.
    Unknown,
}

/// The family of the port with this USB id.
pub fn family(usb: Option<UsbId>) -> Family {
    let Some(UsbId { vid, pid }) = usb else {
        return Family::Unknown;
    };
    match (vid, pid) {
        (0x303a, 0x1001) => Family::EspUsbJtag,
        (0x10c4, 0xea60) | (0x1a86, 0x7523) | (0x1a86, 0x55d4) | (0x0403, 0x6001) => {
            Family::UartBridge
        }
        (0x2e8a, 0x0009) => Family::RpBase,
        (0x1366, 0x0105) | (0x1fc9, 0x0094) => Family::Rt1050,
        (0x0d28, 0x0204) => Family::Microbit,
        (0x2341, _) | (0x2a03, _) | (0x239a, _) => Family::Touch1200,
        _ => Family::Unknown,
    }
}

impl Family {
    /// The family in a sentence.
    pub const fn describe(self) -> &'static str {
        match self {
            Family::EspUsbJtag => "ESP32-S3 or ESP32-C3 on its USB-Serial-JTAG port",
            Family::UartBridge => "USB-UART bridge, as on ESP32 and ESP8266 development boards",
            Family::RpBase => "Raspberry Pi Pico or Pico 2 running rp-base",
            Family::Rt1050 => "MIMXRT1050-EVKB",
            Family::Microbit => "micro:bit v2 (DAPLink serial port)",
            Family::Touch1200 => "Arduino or Adafruit native-USB board",
            Family::Unknown => "unrecognised USB serial port",
        }
    }

    /// Whether an Espressif ROM bootloader may be listening on the port.
    pub const fn may_be_esp(self) -> bool {
        matches!(self, Family::EspUsbJtag | Family::UartBridge | Family::Unknown)
    }
}

/// How the base firmware gets onto a board that is ready to flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The Espressif ROM bootloader over this serial port. `listening`:
    /// it answered just now; otherwise the client enters it with DTR/RTS.
    EspRom {
        /// The ROM answered `SYNC`.
        listening: bool,
    },
    /// The firmware is expected but silent: a UF2 in BOOTSEL mode.
    RpBootsel,
    /// The firmware's own A/B updater, once it answers again.
    AbUpdate,
    /// The repository's nRF52 bootloader, entered by hand.
    Nrf52Reflash,
    /// A 1200-baud-touch bootloader; no base firmware for the board.
    Touch1200,
}

impl Method {
    /// The word after `method:`.
    pub const fn name(self) -> &'static str {
        match self {
            Method::EspRom { .. } => "esp-rom",
            Method::RpBootsel => "rp-bootsel",
            Method::AbUpdate => "ab-update",
            Method::Nrf52Reflash => "nrf52-reflash",
            Method::Touch1200 => "touch-1200",
        }
    }

    /// What to do, in one sentence.
    pub const fn how(self) -> &'static str {
        match self {
            Method::EspRom { listening: true } => {
                "the ROM bootloader is listening: flash the base firmware over this port now \
                 (the web page's Flash tab, or espflash)"
            }
            Method::EspRom { listening: false } => {
                "flash the base firmware over this port: the flasher enters the ROM bootloader \
                 through DTR/RTS (the web page's Flash tab, or espflash); hold BOOT while \
                 pressing RESET if the board has no auto-reset circuit"
            }
            Method::RpBootsel => {
                "rp-base does not answer: hold BOOTSEL while plugging the board in and copy the \
                 rp-base UF2 onto the drive that appears (the web page's Start tab has the file)"
            }
            Method::AbUpdate => {
                "the firmware does not answer: reset the board, then install with mwctl fw update"
            }
            Method::Nrf52Reflash => {
                "hold button A while pressing reset to enter the bootloader, then install with \
                 mwctl reflash <IMAGE>"
            }
            Method::Touch1200 => {
                "its bootloader starts when the port is opened at 1200 baud, but there is no \
                 middleware base firmware for this board yet"
            }
        }
    }
}

/// What [`probe`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// A base firmware answered `sys/info`.
    Update(Info),
    /// No firmware answered; the base firmware goes on by this method.
    Flash(Method),
    /// Nothing answered and the USB id decides nothing.
    Unknown,
}

/// A probe's result with what it was told about the port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The port's USB id, if given.
    pub usb: Option<UsbId>,
    /// The verdict.
    pub readiness: Readiness,
}

impl Report {
    /// The report as `name: value` lines; the first is `ready: ...`.
    pub fn lines(&self) -> Vec<String> {
        let fam = family(self.usb);
        let mut out = Vec::new();
        let usb = |out: &mut Vec<String>| {
            if let Some(id) = self.usb {
                out.push(format!("usb: {id}"));
            }
        };
        match &self.readiness {
            Readiness::Update(info) => {
                out.push("ready: update".into());
                usb(&mut out);
                out.push(format!("chip: {}", info.chip));
                out.push(format!("firmware: {}", info.firmware));
                out.push(format!("abi: {}", info.abi_version));
                out.push(format!("capacity: {}", info.capacity));
                out.push(format!(
                    "payloads: {}",
                    if info.runs_payloads() { "run" } else { "stored" }
                ));
                out.push(format!(
                    "how: {}",
                    if info.runs_payloads() {
                        "write a payload with mwctl wire or mwctl upload"
                    } else {
                        "this firmware stores payloads but has no runtime to run them; flash a \
                         firmware with the WASM runtime to run a wiring"
                    }
                ));
            }
            Readiness::Flash(method) => {
                out.push("ready: flash".into());
                usb(&mut out);
                out.push(format!("board: {}", fam.describe()));
                out.push(format!("method: {}", method.name()));
                out.push(format!("how: {}", method.how()));
            }
            Readiness::Unknown => {
                out.push("ready: unknown".into());
                usb(&mut out);
                out.push(format!("board: {}", fam.describe()));
                out.push(
                    "how: nothing answered on this port; if it is an ESP board, hold BOOT while \
                     pressing RESET and probe again"
                        .into(),
                );
            }
        }
        out
    }
}

const SLIP_END: u8 = 0xc0;
const ESP_SYNC: u8 = 0x08;

/// The Espressif ROM bootloader's `SYNC` command as one SLIP frame: a
/// request (direction 0, command 8, 36 data bytes, checksum 0) whose data
/// is `07 07 12 20` and 32 bytes of `0x55`. None of the bytes needs a SLIP
/// escape.
pub fn esp_sync() -> Vec<u8> {
    let mut frame = vec![SLIP_END, 0x00, ESP_SYNC, 36, 0, 0, 0, 0, 0, 0x07, 0x07, 0x12, 0x20];
    frame.extend(std::iter::repeat_n(0x55, 32));
    frame.push(SLIP_END);
    frame
}

/// Whether `bytes` hold a ROM reply to `SYNC`: a SLIP frame that starts
/// with direction 1 and command 8 and is at least a reply header long.
pub fn is_esp_sync_reply(bytes: &[u8]) -> bool {
    bytes
        .split(|&b| b == SLIP_END)
        .any(|frame| frame.len() >= 8 && frame[0] == 0x01 && frame[1] == ESP_SYNC)
}

/// The verdict when nothing answered `sys/info`, from the family and
/// whether a ROM answered `SYNC`.
pub fn silent(fam: Family, rom_answered: bool) -> Readiness {
    if rom_answered {
        return Readiness::Flash(Method::EspRom { listening: true });
    }
    match fam {
        Family::EspUsbJtag | Family::UartBridge => {
            Readiness::Flash(Method::EspRom { listening: false })
        }
        Family::RpBase => Readiness::Flash(Method::RpBootsel),
        Family::Rt1050 => Readiness::Flash(Method::AbUpdate),
        Family::Microbit => Readiness::Flash(Method::Nrf52Reflash),
        Family::Touch1200 => Readiness::Flash(Method::Touch1200),
        Family::Unknown => Readiness::Unknown,
    }
}

/// Probes the board on `io`. `wait` bounds each of the two `sys/info`
/// attempts and the wait for a ROM reply. Only a failing port is an error;
/// a silent board is a verdict.
pub fn probe<T: Transport>(io: T, usb: Option<UsbId>, wait: Duration) -> Result<Report, Error> {
    let timeouts = Timeouts { request: wait, retries: 1, ..Timeouts::default() };
    let mut dev = Device::new(Link::new(io, timeouts));
    match dev.info() {
        Ok(info) => return Ok(Report { usb, readiness: Readiness::Update(info) }),
        Err(Error::Io(e)) => return Err(Error::Io(e)),
        Err(_) => {}
    }
    let fam = family(usb);
    let mut rom_answered = false;
    if fam.may_be_esp() {
        let io = dev.link_mut().transport_mut();
        io.write_all(&esp_sync())?;
        let deadline = Instant::now() + wait;
        let mut seen = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || is_esp_sync_reply(&seen) {
                break;
            }
            let n = io.read(&mut buf, left)?;
            seen.extend_from_slice(buf.get(..n).unwrap_or(&[]));
        }
        rom_answered = is_esp_sync_reply(&seen);
    }
    Ok(Report { usb, readiness: silent(fam, rom_answered) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeDevice;
    use std::io;

    const WAIT: Duration = Duration::from_millis(20);

    /// A port nothing answers on, or an Espressif ROM in download mode.
    struct Silent {
        rom: bool,
        written: Vec<u8>,
        reply: Vec<u8>,
    }

    impl Silent {
        fn new(rom: bool) -> Self {
            Silent { rom, written: Vec::new(), reply: Vec::new() }
        }
    }

    impl Transport for Silent {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.written.extend_from_slice(bytes);
            if self.rom && bytes == esp_sync() {
                // The ROM's reply: direction 1, command 8, 4 status bytes.
                self.reply = vec![0xc0, 0x01, 0x08, 0x04, 0x00, 0x07, 0x07, 0x12, 0x20, 0, 0, 0, 0, 0xc0];
            }
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
            if self.reply.is_empty() {
                std::thread::sleep(timeout);
                return Ok(0);
            }
            let n = self.reply.len().min(buf.len());
            buf[..n].copy_from_slice(&self.reply[..n]);
            self.reply.drain(..n);
            Ok(n)
        }
    }

    fn usb(s: &str) -> Option<UsbId> {
        Some(UsbId::parse(s).expect("a usb id"))
    }

    #[test]
    fn rd1_usb_ids_parse_and_name_their_family() {
        assert_eq!(UsbId::parse("303A:1001"), Some(UsbId { vid: 0x303a, pid: 0x1001 }));
        assert_eq!(UsbId::parse("2e8a:9").map(|u| u.to_string()), Some("2e8a:0009".into()));
        for bad in ["", "303a", "303a:", "303a:10011", "zzzz:0001", "303a:1001:1", "+1:2"] {
            assert_eq!(UsbId::parse(bad), None, "{bad}");
        }
        let fam = |s: &str| family(usb(s));
        assert_eq!(fam("303a:1001"), Family::EspUsbJtag);
        for bridge in ["10c4:ea60", "1a86:7523", "1a86:55d4", "0403:6001"] {
            assert_eq!(fam(bridge), Family::UartBridge);
        }
        assert_eq!(fam("2e8a:0009"), Family::RpBase);
        assert_eq!(fam("1366:0105"), Family::Rt1050);
        assert_eq!(fam("1fc9:0094"), Family::Rt1050);
        assert_eq!(fam("0d28:0204"), Family::Microbit);
        assert_eq!(fam("2341:8036"), Family::Touch1200);
        assert_eq!(fam("239a:800b"), Family::Touch1200);
        // The Colorlight's and the KD240's FT4232H is a JTAG adapter too.
        assert_eq!(fam("0403:6011"), Family::Unknown);
        assert_eq!(family(None), Family::Unknown);
    }

    #[test]
    fn rd2_a_base_firmware_that_answers_is_ready_to_update() {
        let report = probe(FakeDevice::new("esp32s3", "0.6.0", 65536, true), usb("303a:1001"), WAIT).unwrap();
        let lines = report.lines();
        assert_eq!(lines[0], "ready: update");
        assert!(lines.contains(&"usb: 303a:1001".to_string()));
        assert!(lines.contains(&"chip: esp32s3".to_string()));
        assert!(lines.contains(&"payloads: run".to_string()));
        assert!(lines.last().unwrap().contains("mwctl wire"));

        let report = probe(FakeDevice::new("esp8266", "0.1.0+no-wasm", 65536, false), None, WAIT).unwrap();
        let lines = report.lines();
        assert_eq!(lines[0], "ready: update");
        assert!(lines.contains(&"payloads: stored".to_string()));
        assert!(lines.last().unwrap().contains("no runtime"));
    }

    #[test]
    fn rd3_a_rom_that_answers_sync_is_ready_to_flash_now() {
        // Whatever the id: an unknown bridge with an ESP in download mode.
        for id in [usb("10c4:ea60"), usb("303a:1001"), None] {
            let report = probe(Silent::new(true), id, WAIT).unwrap();
            assert_eq!(report.readiness, Readiness::Flash(Method::EspRom { listening: true }));
            let lines = report.lines();
            assert_eq!(lines[0], "ready: flash");
            assert!(lines.contains(&"method: esp-rom".to_string()));
            assert!(lines.last().unwrap().contains("is listening"));
        }
    }

    #[test]
    fn rd3_the_sync_frame_and_its_reply_are_the_rom_protocol() {
        let frame = esp_sync();
        assert_eq!(frame.len(), 46);
        assert_eq!(&frame[..13], &[0xc0, 0, 8, 36, 0, 0, 0, 0, 0, 7, 7, 0x12, 0x20]);
        assert!(frame[13..45].iter().all(|&b| b == 0x55));
        assert_eq!(frame[45], 0xc0);
        assert!(!frame[1..45].contains(&0xc0) && !frame[1..45].contains(&0xdb));
        assert!(is_esp_sync_reply(&[0xc0, 1, 8, 4, 0, 7, 7, 0x12, 0x20, 0, 0, 0, 0, 0xc0]));
        // Boot noise before the frame, and several replies, are fine.
        assert!(is_esp_sync_reply(&[0x55, 0xaa, 0xc0, 1, 8, 4, 0, 0, 0, 0, 0, 0xc0, 0xc0]));
        // Our own request echoed back, a short frame and text are not.
        assert!(!is_esp_sync_reply(&frame));
        assert!(!is_esp_sync_reply(&[0xc0, 1, 8, 4, 0xc0]));
        assert!(!is_esp_sync_reply(b"waiting for download\r\n"));
    }

    #[test]
    fn rd4_a_silent_port_is_judged_by_its_family() {
        let verdict = |id: &str| probe(Silent::new(false), usb(id), WAIT).unwrap().readiness;
        assert_eq!(verdict("1a86:7523"), Readiness::Flash(Method::EspRom { listening: false }));
        assert_eq!(verdict("303a:1001"), Readiness::Flash(Method::EspRom { listening: false }));
        assert_eq!(verdict("2e8a:0009"), Readiness::Flash(Method::RpBootsel));
        assert_eq!(verdict("1366:0105"), Readiness::Flash(Method::AbUpdate));
        assert_eq!(verdict("0d28:0204"), Readiness::Flash(Method::Nrf52Reflash));
        assert_eq!(verdict("2341:8036"), Readiness::Flash(Method::Touch1200));
        assert_eq!(verdict("dead:beef"), Readiness::Unknown);
        let lines = probe(Silent::new(false), None, WAIT).unwrap().lines();
        assert_eq!(lines[0], "ready: unknown");
        assert!(lines.last().unwrap().contains("hold BOOT"));
    }

    #[test]
    fn rd4_sync_is_sent_only_where_an_espressif_rom_may_listen() {
        let sent_sync = |id: &str| {
            let mut port = Silent::new(false);
            // `probe` takes the transport by value; lend it one.
            struct Lend<'a>(&'a mut Silent);
            impl Transport for Lend<'_> {
                fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
                    self.0.write_all(b)
                }
                fn read(&mut self, b: &mut [u8], t: Duration) -> io::Result<usize> {
                    self.0.read(b, t)
                }
            }
            probe(Lend(&mut port), usb(id), WAIT).unwrap();
            port.written.windows(4).any(|w| w == [0x07, 0x07, 0x12, 0x20])
        };
        assert!(sent_sync("10c4:ea60"));
        assert!(sent_sync("dead:beef"));
        assert!(!sent_sync("2e8a:0009"));
        assert!(!sent_sync("0d28:0204"));
        assert!(!sent_sync("2341:8036"));
    }
}
