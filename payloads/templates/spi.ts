// SPI loopback template (AssemblyScript) for the middleware payload ABI 2.
//
// Wiring: jumper MOSI (GPIO 11) to MISO (GPIO 13). SCK is GPIO 12 and CS
// GPIO 10, the ESP32-S3 SPI2 defaults. Every 100 ms hardware timer 0 starts
// a 16-byte DMA transfer of a counter pattern; when the DmaDone event
// arrives the received bytes are read back and compared. The running
// counts are published as ASCII decimal on "app/spi/ok" and "app/spi/err".
//
// DMA model: spi_transfer copies the transmit bytes into a firmware-owned
// buffer and returns 0 at once (or -7 Busy while a transfer is in flight).
// Completion arrives as on_irq(kind = 3 DmaDone, source = 0 SPI0, value =
// bytes or a negative error, timeUs); dma_read then copies the received
// bytes into this module's memory. Guest memory is never a DMA target.
//
// Build: asc spi.ts --runtime stub --use abort= -O \
//          --initialMemory 1 --maximumMemory 1 --outFile spi.wasm

@external("mw", "log")
declare function mw_log(ptr: usize, len: i32): void;
@external("mw", "publish")
declare function mw_publish(keyPtr: usize, keyLen: i32, dataPtr: usize, dataLen: i32): i32;
@external("mw", "timer_start")
declare function mw_timer_start(id: i32, periodUs: i32, periodic: i32): i32;
// Unused pins are -1. mode: 0..3 (CPOL << 1 | CPHA).
@external("mw", "spi_open")
declare function mw_spi_open(bus: i32, sck: i32, mosi: i32, miso: i32, cs: i32, freqKhz: i32, mode: i32): i32;
@external("mw", "spi_transfer")
declare function mw_spi_transfer(bus: i32, txPtr: usize, txLen: i32, rxLen: i32): i32;
@external("mw", "dma_read")
declare function mw_dma_read(channel: i32, ptr: usize, len: i32): i32;

const KIND_TIMER: i32 = 2;
const KIND_DMA_DONE: i32 = 3;
const DMA_SPI0: i32 = 0;

const BUS: i32 = 0;
const SCK: i32 = 12;
const MOSI: i32 = 11;
const MISO: i32 = 13;
const CS: i32 = 10;
const FREQ_KHZ: i32 = 1000;
const TIMER: i32 = 0;
const PERIOD_US: i32 = 100000;
const LEN: i32 = 16;
// Busy periods after which the DmaDone is assumed lost to queue overflow.
const STALL_PERIODS: i32 = 10;

const OK_KEY = String.UTF8.encode("app/spi/ok");
const ERR_KEY = String.UTF8.encode("app/spi/err");
const STARTED = String.UTF8.encode("spi: started (jumper MOSI to MISO)");
const REFUSED = String.UTF8.encode("spi: open or timer refused");
const TX: usize = memory.data(LEN);
const RX: usize = memory.data(LEN);
const DIGITS: usize = memory.data(10);

let seq: u8 = 0;
let busy = false;
let busyPeriods: i32 = 0;
let ok: u32 = 0;
let err: u32 = 0;

function logBytes(b: ArrayBuffer): void {
  mw_log(changetype<usize>(b), b.byteLength);
}

function publishCount(key: ArrayBuffer, n: u32): void {
  let p = DIGITS + 10;
  do {
    p--;
    store<u8>(p, <u8>(0x30 + n % 10));
    n /= 10;
  } while (n != 0);
  mw_publish(changetype<usize>(key), key.byteLength, p, <i32>(DIGITS + 10 - p));
}

function record(good: bool): void {
  if (good) publishCount(OK_KEY, ++ok);
  else publishCount(ERR_KEY, ++err);
}

export function init(): void {
  const opened = mw_spi_open(BUS, SCK, MOSI, MISO, CS, FREQ_KHZ, 0);
  const started = mw_timer_start(TIMER, PERIOD_US, 1);
  logBytes(opened < 0 || started < 0 ? REFUSED : STARTED);
}

function onTimer(): void {
  if (busy) {
    if (++busyPeriods < STALL_PERIODS) return;
    busy = false; // completion lost; let the host decide with Busy
    record(false);
  }
  busyPeriods = 0;
  for (let i: i32 = 0; i < LEN; i++) store<u8>(TX + i, <u8>(seq + <u8>i));
  const r = mw_spi_transfer(BUS, TX, LEN, LEN);
  if (r == 0) {
    busy = true;
    seq++;
  } else if (r != -7) {
    record(false); // -7 Busy: the host is still transferring; retry later
  }
}

function onDone(value: i32): void {
  busy = false;
  let good = value == LEN && mw_dma_read(DMA_SPI0, RX, LEN) == LEN;
  for (let i: i32 = 0; good && i < LEN; i++) {
    if (load<u8>(RX + i) != load<u8>(TX + i)) good = false;
  }
  record(good);
}

export function on_irq(kind: i32, source: i32, value: i32, timeUs: i32): void {
  if (kind == KIND_TIMER && source == TIMER) onTimer();
  else if (kind == KIND_DMA_DONE && source == DMA_SPI0) onDone(value);
}
