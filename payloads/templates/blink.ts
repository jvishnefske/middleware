// Blink template (AssemblyScript) for the middleware payload ABI v1.
//
// Toggles GPIO 8 every 500 ms and publishes the level on "app/blink/state"
// as one ASCII byte, "1" or "0".
//
// Build: asc blink.ts --runtime stub --use abort= -O \
//          --initialMemory 1 --maximumMemory 1 --outFile blink.wasm
// (see README.md for why each flag matters).

// ---- Host imports: module "mw", every value an i32. A negative result is
// ---- an error code: -1 invalid argument, -2 out of bounds, -3 no
// ---- resources, -4 unsupported, -5 transport, -6 invalid key.
@external("mw", "log")
declare function mw_log(ptr: usize, len: i32): void;
@external("mw", "publish")
declare function mw_publish(keyPtr: usize, keyLen: i32, dataPtr: usize, dataLen: i32): i32;
@external("mw", "set_tick")
declare function mw_set_tick(periodMs: i32): i32;
@external("mw", "gpio_output")
declare function mw_gpio_output(pin: i32): i32;
@external("mw", "gpio_write")
declare function mw_gpio_write(pin: i32, level: i32): i32;

const PIN: i32 = 8;
const PERIOD_MS: i32 = 500;

// UTF-8 copies of the strings, made once when the module starts. With the
// stub runtime memory is never freed, so nothing is encoded per tick.
const STATE_KEY = String.UTF8.encode("app/blink/state");
const STARTED = String.UTF8.encode("blink: started");
const REFUSED = String.UTF8.encode("blink: gpio refused");
// One byte of static memory for the published level.
const LEVEL_BUF: usize = memory.data(1);

let level: i32 = 0;

function logBytes(b: ArrayBuffer): void {
  mw_log(changetype<usize>(b), b.byteLength);
}

export function init(): void {
  if (mw_gpio_output(PIN) < 0) logBytes(REFUSED);
  mw_set_tick(PERIOD_MS);
  logBytes(STARTED);
}

export function tick(nowMs: i32): void {
  level ^= 1;
  mw_gpio_write(PIN, level);
  store<u8>(LEVEL_BUF, <u8>(0x30 + level)); // '0' + level
  mw_publish(changetype<usize>(STATE_KEY), STATE_KEY.byteLength, LEVEL_BUF, 1);
}
