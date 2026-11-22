//! micro:bit v2 bootloader: hardware side. The frame format, flash map and
//! boot rules are documented in the crate root (`src/lib.rs`).
//!
//! Peripherals are driven through their register addresses (nRF52833 product
//! specification; the offsets match the nrf-pac 0.4 register map). The
//! bootloader has no interrupts and no executor: UARTE0 runs one byte per DMA
//! transfer, and the CPU polls the end events.
#![no_std]
#![no_main]

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::slice;

use bootloader_nrf52::{
    digest_matches, erase_end, in_app, select_board, vector_table_valid, Board, Header, Parser,
    Step, Word, WordStager, ACK, APP_BASE, CAN, CONFIG_BASE, CONFIG_LEN, NAK, PAGE_SIZE,
};
use cortex_m_rt::entry;
use panic_halt as _;

// Register bases.
const CLOCK: usize = 0x4000_0000;
const UARTE0: usize = 0x4000_2000;
const NVMC: usize = 0x4001_E000;
const P0: usize = 0x5000_0000;
const P1: usize = 0x5000_0300;
const SCB_VTOR: usize = 0xE000_ED08;

// CLOCK.
const CLOCK_TASKS_HFCLKSTART: usize = CLOCK + 0x000;
const CLOCK_EVENTS_HFCLKSTARTED: usize = CLOCK + 0x100;

// UARTE0.
const UARTE_TASKS_STARTRX: usize = UARTE0 + 0x000;
const UARTE_TASKS_STARTTX: usize = UARTE0 + 0x008;
const UARTE_EVENTS_ENDRX: usize = UARTE0 + 0x110;
const UARTE_EVENTS_ENDTX: usize = UARTE0 + 0x120;
const UARTE_ENABLE: usize = UARTE0 + 0x500;
const UARTE_PSEL_TXD: usize = UARTE0 + 0x50C;
const UARTE_PSEL_RXD: usize = UARTE0 + 0x514;
const UARTE_BAUDRATE: usize = UARTE0 + 0x524;
const UARTE_CONFIG: usize = UARTE0 + 0x56C;
const UARTE_RXD_PTR: usize = UARTE0 + 0x534;
const UARTE_RXD_MAXCNT: usize = UARTE0 + 0x538;
const UARTE_TXD_PTR: usize = UARTE0 + 0x544;
const UARTE_TXD_MAXCNT: usize = UARTE0 + 0x548;

// NVMC.
const NVMC_READY: usize = NVMC + 0x400;
const NVMC_CONFIG: usize = NVMC + 0x504;
const NVMC_ERASEPAGE: usize = NVMC + 0x508;
const NVMC_CONFIG_REN: u32 = 0;
const NVMC_CONFIG_WEN: u32 = 1;
const NVMC_CONFIG_EEN: u32 = 2;

// GPIO. PIN_CNF starts at 0x700, one word per pin.
const GPIO_IN: usize = 0x510;
const GPIO_PIN_CNF: usize = 0x700;

/// UARTE BAUDRATE register value for 115200.
const BAUD_115200: u32 = 0x01D7_E000;
/// Pin configuration: input, connected, pull-up.
const PIN_INPUT_PULLUP: u32 = 0b11 << 2;
/// Pin configuration: output, input disconnected (the TX pin).
const PIN_OUTPUT: u32 = 0b11;

/// Inter-byte timeout inside a frame, counted in polls of the end event.
/// Roughly one second at the 64 MHz core clock. Approximate, since it is a
/// spin count and not a timer.
const RX_SPINS: u32 = 20_000_000;

/// One-byte DMA buffers. The UARTE writes into `RX_BYTE` and reads from
/// `TX_BYTE`. Both must live in RAM.
static mut RX_BYTE: u8 = 0;
static mut TX_BYTE: u8 = 0;

#[inline(always)]
fn rd(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

#[inline(always)]
fn wr(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

/// Reads the application's initial SP and reset vector at `APP_BASE`.
fn app_vectors() -> (u32, u32) {
    (rd(APP_BASE as usize), rd(APP_BASE as usize + 4))
}

/// The GPIO port block and pin number for a PSEL value (`port * 32 + pin`).
fn gpio_of(psel: u32) -> (usize, u32) {
    if psel >= 32 { (P1, psel - 32) } else { (P0, psel) }
}

/// The pin configuration register address of a PSEL pin.
fn pin_cnf(psel: u32) -> usize {
    let (port, pin) = gpio_of(psel);
    port + GPIO_PIN_CNF + 4 * pin as usize
}

/// The config record's bytes, read straight from flash (memory mapped).
fn config_bytes() -> [u8; CONFIG_LEN] {
    let mut out = [0u8; CONFIG_LEN];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = unsafe { read_volatile((CONFIG_BASE as usize + i) as *const u8) };
    }
    out
}

/// True while the board's button (active low) is held.
fn button_held(board: &Board) -> bool {
    let (port, pin) = gpio_of(board.button);
    wr(pin_cnf(board.button), PIN_INPUT_PULLUP);
    for _ in 0..1000 {
        cortex_m::asm::nop();
    }
    rd(port + GPIO_IN) & (1 << pin) == 0
}

/// Starts the 32 MHz crystal so the UART baud rate is accurate. Bounded wait:
/// if the crystal does not start the RC oscillator still runs, slower but
/// usable.
fn start_hfclk() {
    wr(CLOCK_EVENTS_HFCLKSTARTED, 0);
    wr(CLOCK_TASKS_HFCLKSTART, 1);
    for _ in 0..1_000_000 {
        if rd(CLOCK_EVENTS_HFCLKSTARTED) != 0 {
            break;
        }
    }
}

fn uart_init(board: &Board) {
    wr(pin_cnf(board.tx), PIN_OUTPUT); // TX, idle high
    wr(pin_cnf(board.rx), 0); // RX, input
    wr(UARTE_PSEL_TXD, board.tx);
    wr(UARTE_PSEL_RXD, board.rx);
    wr(UARTE_BAUDRATE, BAUD_115200);
    wr(UARTE_CONFIG, 0); // 8N1, no hardware flow control
    wr(UARTE_ENABLE, 8);
    arm_rx();
}

/// Arms the receive DMA for one byte.
fn arm_rx() {
    wr(UARTE_EVENTS_ENDRX, 0);
    wr(UARTE_RXD_PTR, addr_of!(RX_BYTE) as u32);
    wr(UARTE_RXD_MAXCNT, 1);
    wr(UARTE_TASKS_STARTRX, 1);
}

/// The next byte from the host. With `spins` set, gives up after that many
/// polls and returns `None`. The DMA stays armed, so the next byte is not
/// lost. The receiver is re-armed right after the byte is copied out, which
/// is well inside one byte time at 115200 baud.
fn rx_byte(spins: Option<u32>) -> Option<u8> {
    let mut n = 0u32;
    loop {
        if rd(UARTE_EVENTS_ENDRX) != 0 {
            let byte = unsafe { read_volatile(addr_of!(RX_BYTE)) };
            arm_rx();
            return Some(byte);
        }
        if let Some(limit) = spins {
            if n >= limit {
                return None;
            }
            n += 1;
        }
    }
}

fn tx(byte: u8) {
    unsafe { write_volatile(addr_of_mut!(TX_BYTE), byte) };
    wr(UARTE_EVENTS_ENDTX, 0);
    wr(UARTE_TXD_PTR, addr_of!(TX_BYTE) as u32);
    wr(UARTE_TXD_MAXCNT, 1);
    wr(UARTE_TASKS_STARTTX, 1);
    while rd(UARTE_EVENTS_ENDTX) == 0 {}
    wr(UARTE_EVENTS_ENDTX, 0);
}

fn nvmc_wait_ready() {
    while rd(NVMC_READY) & 1 == 0 {}
}

/// Erases the page at `addr`. Refuses anything outside the app region or not
/// page aligned.
fn erase_page(addr: u32) -> bool {
    if addr % PAGE_SIZE != 0 || !in_app(addr, PAGE_SIZE) {
        return false;
    }
    wr(NVMC_CONFIG, NVMC_CONFIG_EEN);
    nvmc_wait_ready();
    wr(NVMC_ERASEPAGE, addr);
    nvmc_wait_ready();
    wr(NVMC_CONFIG, NVMC_CONFIG_REN);
    true
}

/// Writes one aligned word at `addr`. Refuses misaligned addresses and
/// anything outside the app region.
fn write_word(addr: u32, value: u32) -> bool {
    if addr % 4 != 0 || !in_app(addr, 4) {
        return false;
    }
    wr(NVMC_CONFIG, NVMC_CONFIG_WEN);
    nvmc_wait_ready();
    unsafe { write_volatile(addr as *mut u32, value) };
    nvmc_wait_ready();
    wr(NVMC_CONFIG, NVMC_CONFIG_REN);
    true
}

/// Erases every page the image of `len` bytes will cover.
fn erase_image(len: u32) -> bool {
    let end = erase_end(len);
    let mut addr = APP_BASE;
    while addr < end {
        if !erase_page(addr) {
            return false;
        }
        addr += PAGE_SIZE;
    }
    true
}

/// State of one image transfer, from an accepted header to the last body byte.
struct Transfer {
    header: Header,
    stager: WordStager,
    /// The first two image words (the vector table), held back in RAM.
    head: [u32; 2],
    /// Body bytes received so far.
    received: u32,
}

impl Transfer {
    fn new(header: Header) -> Self {
        Transfer { header, stager: WordStager::new(), head: [0xFFFF_FFFF; 2], received: 0 }
    }

    /// The held-back vector words as image bytes (little endian).
    fn head_bytes(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&self.head[0].to_le_bytes());
        out[4..].copy_from_slice(&self.head[1].to_le_bytes());
        out
    }
}

/// Places a completed word. The first two words (the vector table) are held
/// in `head` and written only once the digest matches.
fn place(word: Word, head: &mut [u32; 2]) -> bool {
    if word.index < 2 {
        head[word.index as usize] = word.value;
        true
    } else {
        write_word(APP_BASE + 4 * word.index, word.value)
    }
}

/// Verifies the whole body and, only if it matches, commits the vector words.
/// The digest covers the image as the header describes it: the held-back head
/// bytes come from RAM, the rest is read back from flash. Returns the reply.
fn finish_image(t: &Transfer) -> u8 {
    let len = t.header.len;
    let head_bytes = t.head_bytes();
    let head_len = len.min(8);
    let tail = unsafe {
        slice::from_raw_parts((APP_BASE + head_len) as *const u8, (len - head_len) as usize)
    };
    if !digest_matches(&t.header.sha256, &[&head_bytes[..head_len as usize], tail]) {
        erase_page(APP_BASE);
        return NAK;
    }
    // Only words that carry image bytes are written.
    for (i, &value) in t.head.iter().enumerate() {
        let addr = APP_BASE + 4 * i as u32;
        if 4 * (i as u32) >= len {
            break;
        }
        if !write_word(addr, value) || rd(addr as usize) != value {
            erase_page(APP_BASE);
            return NAK;
        }
    }
    ACK
}

/// Serves the frame protocol until reset. Never returns.
fn reflash() -> ! {
    let mut parser = Parser::new();
    let mut transfer: Option<Transfer> = None;

    loop {
        let spins = if parser.in_frame() { Some(RX_SPINS) } else { None };
        let step = match rx_byte(spins) {
            Some(byte) => parser.push(byte),
            None => parser.timeout(),
        };
        match step {
            Step::Pending => {}
            Step::BadHeader => tx(NAK),
            Step::Timeout => {
                // Abandoned. Erase the app if any body byte reached flash, so
                // the next reset stays in reflash mode.
                if let Some(t) = transfer.take() {
                    if t.received > 0 {
                        erase_page(APP_BASE);
                    }
                }
                tx(CAN);
            }
            Step::Header(header) => {
                if erase_image(header.len) {
                    transfer = Some(Transfer::new(header));
                    tx(ACK);
                } else {
                    parser.abort();
                    tx(NAK);
                }
            }
            Step::Body { byte, last } => {
                let mut ok = false;
                if let Some(t) = transfer.as_mut() {
                    ok = true;
                    t.received += 1;
                    if let Some(word) = t.stager.push(byte) {
                        ok = place(word, &mut t.head);
                    }
                    if last && ok {
                        if let Some(word) = t.stager.finish() {
                            ok = place(word, &mut t.head);
                        }
                    }
                }
                if last || !ok {
                    parser.abort();
                    let reply = match transfer.take() {
                        Some(t) if ok => finish_image(&t),
                        _ => {
                            erase_page(APP_BASE);
                            NAK
                        }
                    };
                    tx(reply);
                }
            }
        }
    }
}

/// Hands control to the application at `APP_BASE`.
fn jump_to_app(sp: u32, reset: u32) -> ! {
    cortex_m::interrupt::disable();
    // Stop the console receiver so the app starts with the UART quiet.
    wr(UARTE_ENABLE, 0);
    wr(SCB_VTOR, APP_BASE);
    unsafe { cortex_m::asm::bootstrap(sp as *const u32, reset as *const u32) }
}

#[entry]
fn main() -> ! {
    start_hfclk();

    let board = select_board(&config_bytes());
    let (sp, reset) = app_vectors();
    if !button_held(&board) && vector_table_valid(sp, reset) {
        jump_to_app(sp, reset);
    }

    uart_init(&board);
    reflash();
}
