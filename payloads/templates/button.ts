// Button template (AssemblyScript) for the middleware payload ABI 2.
//
// A falling edge on GPIO 0 (the BOOT button of ESP32-S3 dev boards, input
// with pull-up) toggles the LED on GPIO 8 and publishes the press count on
// "app/button/count" as ASCII decimal. Presses within 20 ms of the last
// accepted one are contact bounce and are ignored.
//
// Interrupt model: the firmware's ISR only timestamps the edge and queues
// it; the main loop later calls on_irq(kind, source, value, timeUs). This
// code never runs inside an ISR. A full queue drops events and counts them;
// irq_overflow() returns that count.
//
// Build: asc button.ts --runtime stub --use abort= -O \
//          --initialMemory 1 --maximumMemory 1 --outFile button.wasm

@external("mw", "log")
declare function mw_log(ptr: usize, len: i32): void;
@external("mw", "publish")
declare function mw_publish(keyPtr: usize, keyLen: i32, dataPtr: usize, dataLen: i32): i32;
@external("mw", "gpio_input")
declare function mw_gpio_input(pin: i32, pull: i32): i32;
@external("mw", "gpio_output")
declare function mw_gpio_output(pin: i32): i32;
@external("mw", "gpio_write")
declare function mw_gpio_write(pin: i32, level: i32): i32;
// edge: 0 off, 1 rising, 2 falling, 3 any. The pin must be an input.
@external("mw", "gpio_irq")
declare function mw_gpio_irq(pin: i32, edge: i32): i32;
@external("mw", "irq_overflow")
declare function mw_irq_overflow(): i32;

// on_irq kinds.
const KIND_GPIO: i32 = 1;

const BUTTON_PIN: i32 = 0;
const LED_PIN: i32 = 8;
const PULL_UP: i32 = 1;
const EDGE_FALLING: i32 = 2;
const DEBOUNCE_US: u32 = 20000;

const COUNT_KEY = String.UTF8.encode("app/button/count");
const STARTED = String.UTF8.encode("button: started");
const REFUSED = String.UTF8.encode("button: gpio refused");
const DROPPED = String.UTF8.encode("button: events dropped");
// Ten bytes of static memory for the decimal count.
const DIGITS: usize = memory.data(10);

let count: u32 = 0;
let led: i32 = 0;
let lastUs: u32 = 0;
let pressedOnce = false;

function logBytes(b: ArrayBuffer): void {
  mw_log(changetype<usize>(b), b.byteLength);
}

// Writes n in decimal at the end of DIGITS; returns the first digit.
function utoa(n: u32): usize {
  let p = DIGITS + 10;
  do {
    p--;
    store<u8>(p, <u8>(0x30 + n % 10));
    n /= 10;
  } while (n != 0);
  return p;
}

export function init(): void {
  if (mw_gpio_input(BUTTON_PIN, PULL_UP) < 0 || mw_gpio_irq(BUTTON_PIN, EDGE_FALLING) < 0) {
    logBytes(REFUSED);
  }
  mw_gpio_output(LED_PIN);
  logBytes(STARTED);
}

export function on_irq(kind: i32, source: i32, value: i32, timeUs: i32): void {
  if (kind != KIND_GPIO || source != BUTTON_PIN || value != 0) return;
  const t = <u32>timeUs;
  // Wrapping subtraction: correct across the 71-minute timer wrap.
  if (pressedOnce && t - lastUs < DEBOUNCE_US) return;
  pressedOnce = true;
  lastUs = t;
  count++;
  led ^= 1;
  mw_gpio_write(LED_PIN, led);
  const start = utoa(count);
  mw_publish(changetype<usize>(COUNT_KEY), COUNT_KEY.byteLength, start, <i32>(DIGITS + 10 - start));
  if (mw_irq_overflow() > 0) logBytes(DROPPED);
}
