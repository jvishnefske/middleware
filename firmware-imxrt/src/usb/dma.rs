//! The controller's DMA structures over the `AtomicU32` memory model: the
//! endpoint queue head (dQH) and the device transfer descriptor (dTD).
//!
//! The queue head list is [`QH_COUNT`] heads of [`QH_WORDS`] words, endpoint
//! `n` OUT at index `2n` and IN at `2n + 1` ([`Ep::qh_index`]), at a
//! [`QH_LIST_ALIGN`]-byte aligned address written to `ENDPTLISTADDR`
//! ([`super::regs::endpointlistaddr`]). The dTD pool is any number of
//! [`DTD_WORDS`]-word descriptors at a [`DTD_ALIGN`]-byte aligned address;
//! the shell knows the pool's bus address and passes it to
//! [`dtd_address`] to name a descriptor to the controller.
//!
//! dQH layout assumed (word, bits):
//!
//! | word | field |
//! |------|-------|
//! | 0    | capabilities: `Mult` 31:30, `ZLT` 29 (1 = no automatic zero-length termination), max packet length 26:16, `IOS` 15 (interrupt on setup) |
//! | 1    | current dTD pointer 31:5 (written by the controller) |
//! | 2    | overlay: next dTD pointer 31:5, `T` 0 (terminate) |
//! | 3    | overlay: token (as the dTD's) |
//! | 4–8  | overlay: buffer pointers 0–4 |
//! | 9    | reserved |
//! | 10–11| setup buffer, the 8 bytes of the last setup packet, little-endian (byte offset 0x28) |
//! | 12–15| reserved (zeroed here, unused) |
//!
//! dTD layout assumed (word, bits):
//!
//! | word | field |
//! |------|-------|
//! | 0    | next dTD pointer 31:5, `T` 0 (terminate) |
//! | 1    | token: total bytes 30:16, `IOC` 15, `MultO` 11:10, status 7:0 (`Active` 7, `Halted` 6, `Data buffer error` 5, `Transaction error` 3) |
//! | 2    | buffer pointer 0: the buffer's address (page 31:12 and offset 11:0) |
//! | 3–6  | buffer pointers 1–4: the following 4 KiB pages (bits 31:12) |
//! | 7    | reserved (zeroed here) |
//!
//! Every write ends with the word that hands the structure over (a dTD's
//! token, a queue head's next pointer), and all accesses are `SeqCst`.

use core::sync::atomic::{AtomicU32, Ordering};

use super::{Ep, Error};

/// Words in one queue head (64 bytes).
pub const QH_WORDS: usize = 16;
/// Queue heads in the list: two per implemented endpoint.
pub const QH_COUNT: usize = 2 * super::ENDPOINTS as usize;
/// Words in the whole queue head list (1 KiB).
pub const QH_LIST_WORDS: usize = QH_WORDS * QH_COUNT;
/// Alignment of the queue head list, in bytes (`ENDPTLISTADDR` 31:11).
pub const QH_LIST_ALIGN: u32 = 2048;
/// Words in one transfer descriptor (32 bytes).
pub const DTD_WORDS: usize = 8;
/// Alignment of a transfer descriptor, in bytes.
pub const DTD_ALIGN: u32 = 32;
/// The bytes one dTD carries from any buffer address: four whole pages
/// past the first, whatever the first page's offset. A page-aligned
/// buffer carries up to 20 KiB ([`dtd_capacity`]).
pub const DTD_MAX_BYTES: u32 = 16 * 1024;
/// Page size of the dTD buffer pointers.
const PAGE: u32 = 4096;
/// Buffer pointers per dTD.
const PAGES: u32 = 5;

/// Terminate bit of a next-dTD pointer.
pub const T_TERMINATE: u32 = 1 << 0;

/// Capabilities: interrupt on setup.
pub const QH_IOS: u32 = 1 << 15;
/// Capabilities: max packet length field shift (11 bits, 26:16).
pub const QH_MAX_PACKET_SHIFT: u32 = 16;
/// Capabilities: max packet length field mask (before the shift).
pub const QH_MAX_PACKET_MASK: u32 = 0x7FF;
/// Capabilities: zero-length termination select (1 = disabled).
pub const QH_ZLT: u32 = 1 << 29;
/// Capabilities: `Mult` field shift (isochronous only; 0 otherwise).
pub const QH_MULT_SHIFT: u32 = 30;

/// Token: total bytes field shift (15 bits, 30:16).
pub const TOKEN_TOTAL_SHIFT: u32 = 16;
/// Token: total bytes field mask (before the shift).
pub const TOKEN_TOTAL_MASK: u32 = 0x7FFF;
/// Token: interrupt on complete.
pub const TOKEN_IOC: u32 = 1 << 15;
/// Token status: active (the controller owns the dTD).
pub const TOKEN_ACTIVE: u32 = 1 << 7;
/// Token status: halted (a serious error or a stall).
pub const TOKEN_HALTED: u32 = 1 << 6;
/// Token status: data buffer error (overrun or underrun of the buffer).
pub const TOKEN_DATA_BUFFER_ERROR: u32 = 1 << 5;
/// Token status: transaction error.
pub const TOKEN_TRANSACTION_ERROR: u32 = 1 << 3;

/// Queue head word: capabilities.
const QH_CAPS: usize = 0;
/// Queue head word: current dTD pointer.
const QH_CURRENT: usize = 1;
/// Queue head word: overlay next dTD pointer.
const QH_NEXT: usize = 2;
/// Queue head word: overlay token.
const QH_TOKEN: usize = 3;
/// Queue head word: first setup buffer word (byte offset 0x28).
const QH_SETUP: usize = 10;

/// dTD word: next pointer.
const DTD_NEXT: usize = 0;
/// dTD word: token.
const DTD_TOKEN: usize = 1;
/// dTD word: buffer pointer 0.
const DTD_BUF0: usize = 2;
/// dTD word: reserved.
const DTD_RESERVED: usize = 7;

/// Reads word `i` of a structure.
fn load(words: &[AtomicU32], i: usize) -> Result<u32, Error> {
    words.get(i).map(|w| w.load(Ordering::SeqCst)).ok_or(Error::ShortMemory)
}

/// Writes word `i` of a structure.
fn store(words: &[AtomicU32], i: usize, value: u32) -> Result<(), Error> {
    words.get(i).map(|w| w.store(value, Ordering::SeqCst)).ok_or(Error::ShortMemory)
}

/// How a queue head is set up: its capabilities word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QhConfig {
    /// Max packet length, 0 to 1024 (the field is 11 bits).
    pub max_packet: u16,
    /// Interrupt on setup (endpoint 0 OUT only).
    pub ios: bool,
    /// Automatic zero-length termination disabled (`ZLT` = 1): the link
    /// decides zero-length packets itself ([`super::needs_zlp`]).
    pub zlt_disabled: bool,
}

impl QhConfig {
    /// Endpoint 0 OUT: 64-byte packets, interrupt on setup.
    pub const CONTROL_OUT: QhConfig =
        QhConfig { max_packet: super::EP0_MAX_PACKET, ios: true, zlt_disabled: true };
    /// Endpoint 0 IN: 64-byte packets.
    pub const CONTROL_IN: QhConfig =
        QhConfig { max_packet: super::EP0_MAX_PACKET, ios: false, zlt_disabled: true };

    /// A bulk or interrupt endpoint with this max packet length.
    pub const fn data(max_packet: u16) -> QhConfig {
        QhConfig { max_packet, ios: false, zlt_disabled: true }
    }

    /// The capabilities word (`Mult` 0: not isochronous).
    pub const fn capabilities(self) -> Result<u32, Error> {
        if self.max_packet > 1024 {
            return Err(Error::TooLong);
        }
        let mut caps = (self.max_packet as u32 & QH_MAX_PACKET_MASK) << QH_MAX_PACKET_SHIFT;
        if self.ios {
            caps |= QH_IOS;
        }
        if self.zlt_disabled {
            caps |= QH_ZLT;
        }
        Ok(caps)
    }

    /// Decodes a capabilities word (the `Mult` field is ignored).
    pub const fn from_capabilities(caps: u32) -> QhConfig {
        QhConfig {
            max_packet: ((caps >> QH_MAX_PACKET_SHIFT) & QH_MAX_PACKET_MASK) as u16,
            ios: caps & QH_IOS != 0,
            zlt_disabled: caps & QH_ZLT != 0,
        }
    }
}

/// `ep`'s queue head in the list.
pub fn qh(list: &[AtomicU32], ep: Ep) -> Result<&[AtomicU32], Error> {
    let start = ep.qh_index() * QH_WORDS;
    list.get(start..start + QH_WORDS).ok_or(Error::ShortMemory)
}

/// Sets a queue head up idle: the capabilities word, no current dTD, an
/// overlay with the terminate bit and no status, a zero setup buffer.
/// Only while its endpoint is not primed (at init and after a bus reset
/// or flush).
pub fn init_qh(qh: &[AtomicU32], config: QhConfig) -> Result<(), Error> {
    let caps = config.capabilities()?;
    if qh.len() < QH_WORDS {
        return Err(Error::ShortMemory);
    }
    for i in 0..QH_WORDS {
        let value = match i {
            QH_CAPS => caps,
            QH_NEXT => T_TERMINATE,
            _ => 0,
        };
        store(qh, i, value)?;
    }
    Ok(())
}

/// The queue head's capabilities, as set by [`init_qh`].
pub fn qh_config(qh: &[AtomicU32]) -> Result<QhConfig, Error> {
    load(qh, QH_CAPS).map(QhConfig::from_capabilities)
}

/// The current dTD pointer the controller last wrote (bits 4:0 dropped).
pub fn qh_current_dtd(qh: &[AtomicU32]) -> Result<u32, Error> {
    load(qh, QH_CURRENT).map(|p| p & !(DTD_ALIGN - 1))
}

/// Points an idle queue head at the dTD at bus address `dtd_addr` (the
/// "linked list empty" case): the overlay's next pointer with `T` clear,
/// in one word write, after clearing `Active` and `Halted` in the overlay
/// token (left from a previous error). The shell then writes
/// [`Ep::bit`] to `ENDPTPRIME`.
pub fn prime_qh(qh: &[AtomicU32], dtd_addr: u32) -> Result<(), Error> {
    if dtd_addr & (DTD_ALIGN - 1) != 0 {
        return Err(Error::Misaligned);
    }
    let token = load(qh, QH_TOKEN)?;
    store(qh, QH_TOKEN, token & !(TOKEN_ACTIVE | TOKEN_HALTED))?;
    store(qh, QH_NEXT, dtd_addr)
}

/// [`prime_qh`] on `ep`'s queue head in the list, returning the value to
/// write to `ENDPTPRIME`.
pub fn prime(list: &[AtomicU32], ep: Ep, dtd_addr: u32) -> Result<u32, Error> {
    prime_qh(qh(list, ep)?, dtd_addr)?;
    Ok(ep.bit())
}

/// The 8 setup buffer bytes (queue head offset 0x28), little-endian as the
/// controller stores them. Read under the setup tripwire
/// ([`super::control::SetupTripwire`]).
pub fn qh_setup_bytes(qh: &[AtomicU32]) -> Result<[u8; 8], Error> {
    let lo = load(qh, QH_SETUP)?.to_le_bytes();
    let hi = load(qh, QH_SETUP + 1)?.to_le_bytes();
    Ok([lo[0], lo[1], lo[2], lo[3], hi[0], hi[1], hi[2], hi[3]])
}

/// Descriptor `index` of the pool.
pub fn dtd(pool: &[AtomicU32], index: usize) -> Result<&[AtomicU32], Error> {
    let start = index.checked_mul(DTD_WORDS).ok_or(Error::ShortMemory)?;
    let end = start.checked_add(DTD_WORDS).ok_or(Error::ShortMemory)?;
    pool.get(start..end).ok_or(Error::ShortMemory)
}

/// The bus address of descriptor `index` in a pool at `pool_base`.
pub fn dtd_address(pool_base: u32, index: usize) -> Result<u32, Error> {
    if pool_base & (DTD_ALIGN - 1) != 0 {
        return Err(Error::Misaligned);
    }
    let index = u32::try_from(index).map_err(|_| Error::ShortMemory)?;
    index
        .checked_mul(DTD_ALIGN)
        .and_then(|off| pool_base.checked_add(off))
        .ok_or(Error::ShortMemory)
}

/// The most bytes one dTD carries from a buffer at `buffer`: five 4 KiB
/// pages less the first page's offset.
pub const fn dtd_capacity(buffer: u32) -> u32 {
    PAGES * PAGE - (buffer & (PAGE - 1))
}

/// One transfer for a dTD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    /// Bus address of the data (any alignment).
    pub buffer: u32,
    /// Bytes to send or the most to receive (0 for a zero-length packet).
    pub len: u32,
    /// Interrupt on complete: sets the endpoint's `ENDPTCOMPLETE` bit and
    /// `USBSTS.UI` when the dTD retires.
    pub ioc: bool,
    /// Bus address of the next dTD, or `None` to end the list here.
    pub next: Option<u32>,
}

/// The token of a fresh dTD: total bytes, `IOC`, `Active`.
pub const fn token(len: u32, ioc: bool) -> u32 {
    let mut t = ((len & TOKEN_TOTAL_MASK) << TOKEN_TOTAL_SHIFT) | TOKEN_ACTIVE;
    if ioc {
        t |= TOKEN_IOC;
    }
    t
}

/// Writes a dTD for `transfer`: next pointer, buffer pointers 0–4 (the
/// address, then each following page), reserved word, and the token last,
/// `Active`, so the controller sees a whole descriptor.
pub fn encode_dtd(dtd: &[AtomicU32], transfer: Transfer) -> Result<(), Error> {
    if transfer.len > dtd_capacity(transfer.buffer) {
        return Err(Error::TooLong);
    }
    let next = match transfer.next {
        Some(addr) if addr & (DTD_ALIGN - 1) != 0 => return Err(Error::Misaligned),
        Some(addr) => addr,
        None => T_TERMINATE,
    };
    if dtd.len() < DTD_WORDS {
        return Err(Error::ShortMemory);
    }
    store(dtd, DTD_NEXT, next)?;
    let page = transfer.buffer & !(PAGE - 1);
    store(dtd, DTD_BUF0, transfer.buffer)?;
    for i in 1..PAGES {
        store(dtd, DTD_BUF0 + i as usize, page.wrapping_add(i * PAGE))?;
    }
    store(dtd, DTD_RESERVED, 0)?;
    store(dtd, DTD_TOKEN, token(transfer.len, transfer.ioc))
}

/// A dTD's (or a queue head overlay's) token, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtdStatus {
    /// The controller still owns the descriptor.
    pub active: bool,
    /// Halted: a serious error, or the endpoint stalled.
    pub halted: bool,
    /// Data buffer error.
    pub data_buffer_error: bool,
    /// Transaction error.
    pub transaction_error: bool,
    /// Bytes not transferred (the total bytes field the controller counts
    /// down).
    pub remaining: u32,
}

/// How a transfer ended, from its dTD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Still active: not retired yet.
    Pending,
    /// Retired without error; `transferred` bytes moved (for an OUT
    /// transfer, the length of what the host sent).
    Done {
        /// Bytes moved.
        transferred: u32,
    },
    /// Retired with an error (the endpoint must be flushed and reprimed;
    /// a halt on a stalled endpoint is reported the same way).
    Failed(DtdStatus),
}

impl DtdStatus {
    /// Decodes a token.
    pub const fn from_token(token: u32) -> DtdStatus {
        DtdStatus {
            active: token & TOKEN_ACTIVE != 0,
            halted: token & TOKEN_HALTED != 0,
            data_buffer_error: token & TOKEN_DATA_BUFFER_ERROR != 0,
            transaction_error: token & TOKEN_TRANSACTION_ERROR != 0,
            remaining: (token >> TOKEN_TOTAL_SHIFT) & TOKEN_TOTAL_MASK,
        }
    }

    /// Whether an error bit is set.
    pub const fn failed(self) -> bool {
        self.halted || self.data_buffer_error || self.transaction_error
    }

    /// How a transfer of `len` bytes stands. An error wins over `Active`
    /// (a halted descriptor is retired whatever its active bit says).
    pub const fn completion(self, len: u32) -> Completion {
        if self.failed() {
            Completion::Failed(self)
        } else if self.active {
            Completion::Pending
        } else {
            Completion::Done { transferred: len.saturating_sub(self.remaining) }
        }
    }
}

/// Reads a dTD's status.
pub fn dtd_status(dtd: &[AtomicU32]) -> Result<DtdStatus, Error> {
    load(dtd, DTD_TOKEN).map(DtdStatus::from_token)
}

/// Reads a queue head overlay's status (the dTD the controller is
/// working on, as it last copied it).
pub fn qh_status(qh: &[AtomicU32]) -> Result<DtdStatus, Error> {
    load(qh, QH_TOKEN).map(DtdStatus::from_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::Dir;

    fn mem<const N: usize>() -> [AtomicU32; N] {
        core::array::from_fn(|_| AtomicU32::new(0xDEAD_BEEF))
    }

    fn word(words: &[AtomicU32], i: usize) -> u32 {
        words.get(i).unwrap().load(Ordering::SeqCst)
    }

    #[test]
    fn fl2_queue_head_list_is_sixteen_64_byte_heads() {
        assert_eq!(QH_WORDS * 4, 64);
        assert_eq!(QH_COUNT, 16);
        assert_eq!(QH_LIST_WORDS * 4, 1024);
        assert_eq!(DTD_WORDS * 4, 32);
        let list = mem::<QH_LIST_WORDS>();
        let in3 = Ep::new(3, Dir::In).unwrap();
        let q = qh(&list, in3).unwrap();
        assert_eq!(q.len(), QH_WORDS);
        assert!(core::ptr::eq(q.as_ptr(), list.get(7 * 16).unwrap()));
        assert_eq!(qh(list.get(..100).unwrap(), in3).err(), Some(Error::ShortMemory));
    }

    #[test]
    fn fl2_control_out_queue_head_interrupts_on_setup_and_disables_zlt() {
        let q = mem::<QH_WORDS>();
        init_qh(&q, QhConfig::CONTROL_OUT).unwrap();
        assert_eq!(word(&q, 0), (64 << 16) | (1 << 15) | (1 << 29));
        assert_eq!(word(&q, 1), 0);
        assert_eq!(word(&q, 2), T_TERMINATE);
        for i in 3..QH_WORDS {
            assert_eq!(word(&q, i), 0, "word {i}");
        }
        assert_eq!(qh_config(&q), Ok(QhConfig::CONTROL_OUT));
        init_qh(&q, QhConfig::data(512)).unwrap();
        assert_eq!(word(&q, 0), (512 << 16) | (1 << 29));
        assert_eq!(qh_config(&q).unwrap().max_packet, 512);
    }

    #[test]
    fn fl2_max_packet_past_1024_and_short_heads_are_refused() {
        assert_eq!(QhConfig::data(1025).capabilities(), Err(Error::TooLong));
        assert_eq!(QhConfig::data(1024).capabilities(), Ok((1024 << 16) | QH_ZLT));
        let short = mem::<15>();
        assert_eq!(init_qh(&short, QhConfig::CONTROL_IN), Err(Error::ShortMemory));
        assert!(short.iter().all(|w| w.load(Ordering::SeqCst) == 0xDEAD_BEEF), "nothing written");
    }

    #[test]
    fn fl2_a_dtd_carries_its_buffer_pages_length_ioc_and_active() {
        let d = mem::<DTD_WORDS>();
        let t = Transfer { buffer: 0x2000_1234, len: 512, ioc: true, next: None };
        encode_dtd(&d, t).unwrap();
        assert_eq!(word(&d, 0), T_TERMINATE);
        assert_eq!(word(&d, 1), (512 << 16) | (1 << 15) | (1 << 7));
        assert_eq!(word(&d, 2), 0x2000_1234);
        assert_eq!(word(&d, 3), 0x2000_2000);
        assert_eq!(word(&d, 4), 0x2000_3000);
        assert_eq!(word(&d, 5), 0x2000_4000);
        assert_eq!(word(&d, 6), 0x2000_5000);
        assert_eq!(word(&d, 7), 0);
        let s = dtd_status(&d).unwrap();
        assert!(s.active && !s.failed());
        assert_eq!(s.remaining, 512);
        assert_eq!(s.completion(512), Completion::Pending);
    }

    #[test]
    fn fl2_a_dtd_links_to_its_next_and_a_zero_length_packet_is_a_dtd() {
        let d = mem::<DTD_WORDS>();
        encode_dtd(&d, Transfer { buffer: 0, len: 0, ioc: false, next: Some(0x2000_0040) }).unwrap();
        assert_eq!(word(&d, 0), 0x2000_0040);
        assert_eq!(word(&d, 1), TOKEN_ACTIVE);
        let t = Transfer { buffer: 0, len: 0, ioc: false, next: Some(0x2000_0044) };
        assert_eq!(encode_dtd(&d, t), Err(Error::Misaligned));
    }

    #[test]
    fn fl2_a_dtd_carries_at_most_five_pages_less_the_offset() {
        assert_eq!(dtd_capacity(0x2000_0000), 20 * 1024);
        assert_eq!(dtd_capacity(0x2000_0FFF), 16 * 1024 + 1);
        assert!(dtd_capacity(u32::MAX) >= DTD_MAX_BYTES);
        let d = mem::<DTD_WORDS>();
        let over = Transfer { buffer: 0x2000_0010, len: 20 * 1024 - 15, ioc: true, next: None };
        assert_eq!(encode_dtd(&d, over), Err(Error::TooLong));
        let fits = Transfer { len: 20 * 1024 - 16, ..over };
        assert_eq!(encode_dtd(&d, fits), Ok(()));
        // Pages past the top of the address space wrap rather than panic.
        let top = Transfer { buffer: 0xFFFF_F000, len: 4096, ioc: false, next: None };
        encode_dtd(&d, top).unwrap();
        assert_eq!(word(&d, 3), 0x0000_0000);
        assert_eq!(encode_dtd(&mem::<7>(), fits), Err(Error::ShortMemory));
    }

    #[test]
    fn fl2_a_completed_dtd_reports_bytes_moved_or_its_error() {
        let done = DtdStatus::from_token((12 << 16) | TOKEN_IOC);
        assert_eq!(done.completion(512), Completion::Done { transferred: 500 });
        // An OUT dTD whose total bytes exceed what was asked still decodes.
        assert_eq!(done.completion(4), Completion::Done { transferred: 0 });
        for bit in [TOKEN_HALTED, TOKEN_DATA_BUFFER_ERROR, TOKEN_TRANSACTION_ERROR] {
            let s = DtdStatus::from_token(bit | TOKEN_ACTIVE | (512 << 16));
            assert!(matches!(s.completion(512), Completion::Failed(_)), "{bit:#x}");
        }
        let s = DtdStatus::from_token(TOKEN_HALTED | TOKEN_TRANSACTION_ERROR);
        assert!(s.halted && s.transaction_error && !s.data_buffer_error && !s.active);
        assert_eq!(DtdStatus::from_token(u32::MAX).remaining, 0x7FFF);
    }

    #[test]
    fn fl2_priming_points_the_overlay_at_the_dtd_and_clears_active_and_halted() {
        let list = mem::<QH_LIST_WORDS>();
        let ep = Ep::new(1, Dir::In).unwrap();
        let q = qh(&list, ep).unwrap();
        init_qh(q, QhConfig::data(512)).unwrap();
        q.get(3).unwrap().store(TOKEN_ACTIVE | TOKEN_HALTED | TOKEN_IOC | (7 << 16), Ordering::SeqCst);
        assert_eq!(prime(&list, ep, 0x2000_0100), Ok(1 << 17));
        assert_eq!(word(q, 2), 0x2000_0100, "next pointer, T clear");
        assert_eq!(word(q, 3), TOKEN_IOC | (7 << 16));
        assert_eq!(qh_status(q).unwrap().completion(7), Completion::Done { transferred: 0 });
        assert_eq!(prime(&list, ep, 0x2000_0108), Err(Error::Misaligned));
        q.get(1).unwrap().store(0x2000_011F, Ordering::SeqCst);
        assert_eq!(qh_current_dtd(q), Ok(0x2000_0100));
    }

    #[test]
    fn fl2_setup_buffer_is_read_little_endian_from_offset_0x28() {
        let q = mem::<QH_WORDS>();
        q.get(10).unwrap().store(0x0005_0500, Ordering::SeqCst);
        q.get(11).unwrap().store(0x0000_0000, Ordering::SeqCst);
        assert_eq!(qh_setup_bytes(&q), Ok([0x00, 0x05, 0x05, 0x00, 0, 0, 0, 0]));
        assert_eq!(qh_setup_bytes(&mem::<11>()), Err(Error::ShortMemory));
    }

    #[test]
    fn fl2_dtd_pool_addresses_are_32_bytes_apart_and_checked() {
        assert_eq!(dtd_address(0x2000_0400, 0), Ok(0x2000_0400));
        assert_eq!(dtd_address(0x2000_0400, 3), Ok(0x2000_0460));
        assert_eq!(dtd_address(0x2000_0410, 0), Err(Error::Misaligned));
        assert_eq!(dtd_address(0xFFFF_FFE0, 1), Err(Error::ShortMemory));
        let pool = mem::<{ 4 * DTD_WORDS }>();
        assert!(core::ptr::eq(dtd(&pool, 3).unwrap().as_ptr(), pool.get(24).unwrap()));
        assert_eq!(dtd(&pool, 4).err(), Some(Error::ShortMemory));
        assert_eq!(dtd(&pool, usize::MAX).err(), Some(Error::ShortMemory));
    }
}
