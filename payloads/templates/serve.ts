// Serve template (AssemblyScript) for the middleware payload ABI 5.
//
// Serves "app/serve_echo" and answers each request with the device's node
// id (8 bytes) followed by the request body, like the Rust `serve_echo`
// example. The host delivers a request by calling mw_alloc(len) for one
// buffer holding a 16-byte meta block, the request and the reply space,
// writing the first two, then calling
// on_request(slot, reqPtr, reqLen, metaPtr, replyPtr, replyCap). The result
// is the reply length, or a negated HandlerError code (-3 = BufferTooSmall).
// A fixed static buffer of MAX_REQUEST_BUFFER bytes is enough, so nothing
// is allocated per request.
//
// Build: asc serve.ts --runtime stub --use abort= -O \
//          --initialMemory 1 --maximumMemory 1 --outFile serve.wasm

@external("mw", "log")
declare function mw_log(ptr: usize, len: i32): void;
@external("mw", "serve")
declare function mw_serve(keyPtr: usize, keyLen: i32): i32;
@external("mw", "node_id")
declare function mw_node_id(ptr: usize): i32;

// 16 (meta) + 512 (request) + 512 (reply).
const MAX_REQUEST_BUFFER: i32 = 1040;
const BUF: usize = memory.data(MAX_REQUEST_BUFFER);
const NODE: usize = memory.data(8);
const BUFFER_TOO_SMALL: i32 = -3;
const NOT_FOUND: i32 = -1;

const KEY = String.UTF8.encode("app/serve_echo");
const SERVING = String.UTF8.encode("serve: serving");
const FAILED = String.UTF8.encode("serve: serve failed");

let slot: i32 = -1;

function logBytes(b: ArrayBuffer): void {
  mw_log(changetype<usize>(b), b.byteLength);
}

export function init(): void {
  mw_node_id(NODE);
  slot = mw_serve(changetype<usize>(KEY), KEY.byteLength);
  logBytes(slot < 0 ? FAILED : SERVING);
}

// Returns the request buffer, or 0 if the request would not fit.
export function mw_alloc(len: i32): usize {
  return len < 0 || len > MAX_REQUEST_BUFFER ? 0 : BUF;
}

export function on_request(s: i32, reqPtr: usize, reqLen: i32, metaPtr: usize, replyPtr: usize, replyCap: i32): i32 {
  if (s != slot) return NOT_FOUND;
  if (reqLen + 8 > replyCap) return BUFFER_TOO_SMALL;
  memory.copy(replyPtr, NODE, 8);
  memory.copy(replyPtr + 8, reqPtr, reqLen);
  return reqLen + 8;
}
