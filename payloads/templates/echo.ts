// Echo template (AssemblyScript) for the middleware payload ABI v1.
//
// Subscribes to "app/echo/in" and republishes each message on
// "app/echo/out". The host delivers a message by calling mw_alloc(len) for
// a buffer in this module's memory, copying the bytes there, then calling
// on_message(slot, ptr, len). A fixed 512-byte (MAX_MESSAGE) static buffer
// is enough, so no allocation happens per message.
//
// Build: asc echo.ts --runtime stub --use abort= -O \
//          --initialMemory 1 --maximumMemory 1 --outFile echo.wasm

@external("mw", "log")
declare function mw_log(ptr: usize, len: i32): void;
@external("mw", "publish")
declare function mw_publish(keyPtr: usize, keyLen: i32, dataPtr: usize, dataLen: i32): i32;
@external("mw", "subscribe")
declare function mw_subscribe(keyPtr: usize, keyLen: i32): i32;

const MAX_MESSAGE: i32 = 512;
const RX: usize = memory.data(MAX_MESSAGE);

const IN_KEY = String.UTF8.encode("app/echo/in");
const OUT_KEY = String.UTF8.encode("app/echo/out");
const STARTED = String.UTF8.encode("echo: started");
const MESSAGE = String.UTF8.encode("echo: message");
const FAILED = String.UTF8.encode("echo: subscribe failed");

// Slot returned by subscribe, or a negative error code.
let inSlot: i32 = -1;

function logBytes(b: ArrayBuffer): void {
  mw_log(changetype<usize>(b), b.byteLength);
}

export function init(): void {
  inSlot = mw_subscribe(changetype<usize>(IN_KEY), IN_KEY.byteLength);
  logBytes(inSlot < 0 ? FAILED : STARTED);
}

// Returns the receive buffer, or 0 if the message would not fit.
export function mw_alloc(len: i32): usize {
  return len < 0 || len > MAX_MESSAGE ? 0 : RX;
}

export function on_message(slot: i32, ptr: usize, len: i32): void {
  if (slot != inSlot) return;
  logBytes(MESSAGE);
  mw_publish(changetype<usize>(OUT_KEY), OUT_KEY.byteLength, ptr, len);
}
