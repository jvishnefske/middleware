//! `wasm-bindgen` exports: the JS-facing surface of the crate.
//!
//! Everything here converts between JS values and the pure modules
//! ([`crate::decoder`], [`crate::encode`], [`crate::loader`],
//! [`crate::pins`], [`crate::machines`], [`crate::recipes`],
//! [`crate::simulator`]); no
//! protocol logic lives in this file. Byte arrays cross as `Uint8Array`;
//! structured results are plain JS objects
//! whose shapes are documented on each function and mirrored in
//! `web/src/protocol/types.ts`.

use crate::decoder::{Event, StreamDecoder};
use crate::loader::{Action, DeviceLoader, Info};
use crate::{encode, loader};
use js_sys::{Array, Object, Reflect, Uint8Array};
use middleware::loader::trust::{self, TrustInfo, TrustState};
use middleware::loader::{CHUNK_MAX, keys};
use middleware::serial::frame::{MAX_RAW_FRAME, MAX_WIRE_FRAME};
use middleware::wasm_abi;
use middleware::wasm_abi::events::{self, DmaChannel, EventKind};
use middleware::wasm_abi::periph::{self, Edge};
use wasm_bindgen::prelude::*;

fn set(obj: &Object, key: &str, value: impl Into<JsValue>) {
    // `Reflect::set` on a plain, unfrozen object cannot fail.
    let _ = Reflect::set(obj, &JsValue::from_str(key), &value.into());
}

fn bytes(data: &[u8]) -> Uint8Array {
    Uint8Array::from(data)
}

fn event_object(e: &Event) -> Object {
    let o = Object::new();
    match e {
        Event::Frame(f) => {
            set(&o, "kind", f.kind.name());
            set(&o, "id", f.id);
            if let Some(k) = &f.key {
                set(&o, "key", k.as_str());
            }
            set(&o, "payload", bytes(&f.payload));
            if let Some(status) = f.status {
                let err = status.err();
                set(&o, "status", encode::status_name(err));
                set(&o, "statusCode", err.map_or(0, |e| e.code()));
            }
        }
        Event::Text(t) => {
            set(&o, "kind", "text");
            set(&o, "text", t.as_str());
        }
        Event::Invalid { reason, len } => {
            set(&o, "kind", "invalid");
            set(&o, "reason", reason.describe());
            set(&o, "len", *len as u32);
        }
    }
    o
}

/// Streaming decoder. `push(bytes)` returns an array of
/// `{kind: "request"|"response"|"publish", id, key?, payload, status?, statusCode?}`,
/// `{kind: "text", text}` and `{kind: "invalid", reason, len}`.
#[wasm_bindgen(js_name = Decoder)]
pub struct JsDecoder(StreamDecoder);

#[wasm_bindgen(js_class = Decoder)]
impl JsDecoder {
    /// A fresh decoder.
    #[wasm_bindgen(constructor)]
    pub fn new() -> JsDecoder {
        JsDecoder(StreamDecoder::new())
    }

    /// Feeds bytes read from the port.
    pub fn push(&mut self, data: &[u8]) -> Array {
        self.0.push(data).iter().map(event_object).collect()
    }
}

impl Default for JsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn js_err(msg: &str) -> JsError {
    JsError::new(msg)
}

/// Wire bytes of a request frame. Throws on an invalid key or oversize.
#[wasm_bindgen(js_name = encodeRequest)]
pub fn encode_request(id: u8, key: &str, payload: &[u8]) -> Result<Vec<u8>, JsError> {
    encode::request(id, key, payload).map_err(|e| js_err(e.describe()))
}

/// Wire bytes of a publish frame.
#[wasm_bindgen(js_name = encodePublish)]
pub fn encode_publish(key: &str, payload: &[u8]) -> Result<Vec<u8>, JsError> {
    encode::publish(key, payload).map_err(|e| js_err(e.describe()))
}

/// Wire bytes of a response frame; `status` 0 is success, otherwise a
/// handler error code (the payload is then ignored).
#[wasm_bindgen(js_name = encodeResponse)]
pub fn encode_response(id: u8, status: u8, payload: &[u8]) -> Result<Vec<u8>, JsError> {
    let result = encode::status_from_code(status).map_or(Ok(payload), Err);
    encode::response(id, result).map_err(|e| js_err(e.describe()))
}

/// Name of a response status code (`"ok"`, `"NotFound"`, ...).
#[wasm_bindgen(js_name = statusName)]
pub fn status_name(code: u8) -> String {
    encode::status_name(encode::status_from_code(code)).into()
}

/// CRC-32/ISO-HDLC of an image, as checked by `sys/loader/commit`.
#[wasm_bindgen]
pub fn crc32(data: &[u8]) -> u32 {
    loader::image_crc32(data)
}

/// Upload plan: `[{key, body: Uint8Array, done}]` for begin, chunks, commit.
/// `chunk` is clamped to `1..=CHUNK_MAX`. The commit is unsigned (empty).
#[wasm_bindgen(js_name = uploadPlan)]
pub fn upload_plan(image: &[u8], chunk: usize) -> Result<Array, JsError> {
    upload_plan_signed(image, chunk, &[])
}

/// As `uploadPlan`, with `commitBody` (empty, or the 68 bytes from
/// `encodeSignedCommit`) as the final `sys/loader/commit` body. Throws on
/// any other commit body length.
#[wasm_bindgen(js_name = uploadPlanSigned)]
pub fn upload_plan_signed(
    image: &[u8],
    chunk: usize,
    commit_body: &[u8],
) -> Result<Array, JsError> {
    if !commit_body.is_empty() && commit_body.len() != trust::SIGNED_COMMIT_LEN {
        return Err(js_err("commit body must be empty or 68 bytes"));
    }
    let steps =
        loader::plan_with_commit(image, chunk, commit_body).map_err(|e| js_err(e.describe()))?;
    Ok(steps
        .iter()
        .map(|s| {
            let o = Object::new();
            set(&o, "key", s.key);
            set(&o, "body", bytes(&s.body));
            set(&o, "done", s.done);
            o
        })
        .collect())
}

/// Decodes a `sys/info` body into
/// `{abiVersion, capacity, status, statusName, chip, firmware}`, plus
/// `node` (16 hex digits) when the device reports its node id, or
/// `undefined` when malformed.
#[wasm_bindgen(js_name = decodeDeviceInfo)]
pub fn decode_device_info(data: &[u8]) -> Option<Object> {
    let info = Info::decode(data)?;
    let o = Object::new();
    set(&o, "abiVersion", info.abi_version);
    set(&o, "capacity", info.capacity);
    set(&o, "status", info.status);
    set(
        &o,
        "statusName",
        loader::status_name(info.status).unwrap_or("unknown"),
    );
    set(&o, "chip", info.chip);
    set(&o, "firmware", info.firmware);
    if let Some(node) = info.node {
        let hex: String = node.iter().map(|b| format!("{b:02x}")).collect();
        set(&o, "node", hex);
    }
    Some(o)
}

/// Encodes a `sys/info` body (for device fakes).
#[wasm_bindgen(js_name = encodeDeviceInfo)]
pub fn encode_device_info(
    abi_version: u16,
    capacity: u32,
    status: u8,
    chip: String,
    firmware: String,
) -> Result<Vec<u8>, JsError> {
    Info {
        abi_version,
        capacity,
        status,
        chip,
        firmware,
        node: None,
    }
    .encode()
    .ok_or_else(|| js_err("invalid device info"))
}

fn pins(mask: u64) -> Array {
    (0u32..64).filter(|n| mask >> n & 1 == 1).map(JsValue::from).collect()
}

/// Decodes a `sys/board` body into `{abiVersion, maxImage, maxMemory, gpio,
/// reserved, inputOnly, strapping, adc, pwmMaxBits, runtime, usbJtag,
/// pwmRefTick, uartTx, uartRx, pwmPins}` (the pin classes as ascending GPIO
/// number arrays, `pwmPins[2·timer + channel]`), or `undefined` when
/// malformed.
#[wasm_bindgen(js_name = decodeBoardInfo)]
pub fn decode_board_info(data: &[u8]) -> Option<Object> {
    use middleware::loader::{BOARD_PWM_REF_TICK, BOARD_RUNTIME, BOARD_USB_JTAG, BoardInfo};
    let b = BoardInfo::decode(data)?;
    let o = Object::new();
    set(&o, "abiVersion", b.abi_version);
    set(&o, "maxImage", b.max_image);
    set(&o, "maxMemory", b.max_memory);
    set(&o, "gpio", pins(b.gpio));
    set(&o, "reserved", pins(b.reserved));
    set(&o, "inputOnly", pins(b.input_only));
    set(&o, "strapping", pins(b.strapping));
    set(&o, "adc", pins(b.adc));
    set(&o, "pwmMaxBits", b.pwm_max_bits);
    set(&o, "runtime", b.flags & BOARD_RUNTIME != 0);
    set(&o, "usbJtag", b.flags & BOARD_USB_JTAG != 0);
    set(&o, "pwmRefTick", b.flags & BOARD_PWM_REF_TICK != 0);
    set(&o, "uartTx", b.uart_tx);
    set(&o, "uartRx", b.uart_rx);
    set(&o, "pwmPins", b.pwm_pins.iter().map(|&p| JsValue::from(p)).collect::<Array>());
    Some(o)
}

/// SHA-256 of `data` (32 bytes), computed by the device loader's hasher.
#[wasm_bindgen]
pub fn sha256(data: &[u8]) -> Vec<u8> {
    loader::sha256(data).to_vec()
}

/// The 54-byte message a payload signature covers:
/// `"mw-payload-v1\0" || len u32 LE || version u32 LE || sha256`. Throws when
/// `sha256` is not 32 bytes. Sign it with Ed25519 (pure, RFC 8032).
#[wasm_bindgen(js_name = signedMessage)]
pub fn signed_message(len: u32, version: u32, sha256: &[u8]) -> Result<Vec<u8>, JsError> {
    loader::signed_message(len, version, sha256).ok_or_else(|| js_err("sha256 must be 32 bytes"))
}

/// The 68-byte signed commit body `[version u32 LE][signature 64]`. Throws
/// when `signature` is not 64 bytes.
#[wasm_bindgen(js_name = encodeSignedCommit)]
pub fn encode_signed_commit(version: u32, signature: &[u8]) -> Result<Vec<u8>, JsError> {
    loader::signed_commit_body(version, signature)
        .ok_or_else(|| js_err("signature must be 64 bytes"))
}

fn trust_info_object(info: &TrustInfo) -> Object {
    let o = Object::new();
    let code = info.code() as u8;
    set(&o, "state", code);
    set(
        &o,
        "stateName",
        loader::trust_state_name(code).unwrap_or("unknown"),
    );
    match info.state {
        TrustState::Unclaimed => set(&o, "minVersion", 0u32),
        TrustState::Claimed { key, min_version } => {
            set(&o, "minVersion", min_version.0);
            set(&o, "key", bytes(&key.0));
        }
    }
    o
}

/// Decodes a `sys/trust/info` body into
/// `{state: 0|1|2, stateName: "unclaimed"|"claimed"|"rekey", minVersion,
/// key?: Uint8Array}` (`key` iff state != 0), or `undefined` when
/// malformed.
#[wasm_bindgen(js_name = decodeTrustInfo)]
pub fn decode_trust_info(data: &[u8]) -> Option<Object> {
    TrustInfo::decode(data).map(|i| trust_info_object(&i))
}

/// Name of a `sys/trust/info` state byte, or `undefined`.
#[wasm_bindgen(js_name = trustStateName)]
pub fn trust_state_name(code: u8) -> Option<String> {
    loader::trust_state_name(code).map(str::to_owned)
}

/// Name of a `sys/payload/status` byte, or `undefined`.
#[wasm_bindgen(js_name = payloadStatusName)]
pub fn payload_status_name(b: u8) -> Option<String> {
    loader::status_name(b).map(str::to_owned)
}

/// Device-side loader running the middleware state machine (for fakes).
#[wasm_bindgen(js_name = DeviceLoader)]
pub struct JsDeviceLoader(DeviceLoader);

#[wasm_bindgen(js_class = DeviceLoader)]
impl JsDeviceLoader {
    /// An idle loader with `capacity` bytes of storage.
    #[wasm_bindgen(constructor)]
    pub fn new(capacity: u32) -> JsDeviceLoader {
        JsDeviceLoader(DeviceLoader::new(capacity))
    }

    /// Whether a transfer is in progress.
    #[wasm_bindgen(getter)]
    pub fn receiving(&self) -> bool {
        self.0.receiving()
    }

    /// Sets the simulated clock (ms since boot); the re-key window expires
    /// against it.
    #[wasm_bindgen(js_name = setNow)]
    pub fn set_now(&mut self, now_ms: f64) {
        // JS numbers: clamp negatives and NaN to 0.
        self.0 = self.0.at(if now_ms > 0.0 { now_ms as u64 } else { 0 });
    }

    /// Simulates physical presence (BOOT held 3 s): opens the re-key window
    /// for 60 s from the current clock.
    #[wasm_bindgen(js_name = assertPresence)]
    pub fn assert_presence(&mut self) {
        self.0 = self.0.assert_presence();
    }

    /// The current trust state, as `decodeTrustInfo` returns it.
    #[wasm_bindgen(getter, js_name = trustInfo)]
    pub fn trust_info(&self) -> Object {
        trust_info_object(&self.0.trust_info())
    }

    /// Handles a request. Returns `{statusCode, status, action?, offset?,
    /// data?, len?, crc?, body?}` where `action` is one of `app`, `info`,
    /// `start`, `stop`, `erase`, `write`, `activate`, `trustInfo` (with the
    /// response `body`), `claim`. Signed commits are checked against the
    /// simulated trust state; a refusal is status `Denied` (5) and no
    /// `activate`.
    pub fn step(&mut self, key: &str, body: &[u8]) -> Object {
        let (next, result) = self.0.step(key, body);
        self.0 = next;
        let o = Object::new();
        let err = result.as_ref().err().copied();
        set(&o, "statusCode", err.map_or(0, |e| e.code()));
        set(&o, "status", encode::status_name(err));
        if let Ok(action) = result {
            let name = match action {
                Action::App => "app",
                Action::Info => "info",
                Action::Board => "board",
                Action::Start => "start",
                Action::Stop => "stop",
                Action::Erase(len) => {
                    set(&o, "len", len);
                    "erase"
                }
                Action::Write(offset, data) => {
                    set(&o, "offset", offset);
                    set(&o, "data", bytes(&data));
                    "write"
                }
                Action::Activate(len, crc) => {
                    set(&o, "len", len);
                    set(&o, "crc", crc);
                    "activate"
                }
                Action::TrustInfo(body) => {
                    set(&o, "body", bytes(&body));
                    "trustInfo"
                }
                Action::Claim => "claim",
            };
            set(&o, "action", name);
        }
        o
    }
}

/// Protocol constants and system keys:
/// `{chunkMax, maxRawFrame, maxWireFrame, signedCommitLen, rekeyWindowMs,
/// presenceHoldMs, keys: {info, board, begin, chunk, commit, start, stop, log,
/// status, trustInfo, trustClaim}}`.
#[wasm_bindgen]
pub fn protocol() -> Object {
    let o = Object::new();
    set(&o, "chunkMax", CHUNK_MAX as u32);
    set(&o, "maxRawFrame", MAX_RAW_FRAME as u32);
    set(&o, "maxWireFrame", MAX_WIRE_FRAME as u32);
    set(&o, "signedCommitLen", trust::SIGNED_COMMIT_LEN as u32);
    set(&o, "rekeyWindowMs", trust::REKEY_WINDOW_MS as f64);
    set(&o, "presenceHoldMs", trust::PRESENCE_HOLD_MS as f64);
    let k = Object::new();
    for (name, key) in [
        ("info", keys::INFO),
        ("board", keys::BOARD),
        ("begin", keys::BEGIN),
        ("chunk", keys::CHUNK),
        ("commit", keys::COMMIT),
        ("start", keys::START),
        ("stop", keys::STOP),
        ("log", keys::LOG),
        ("status", keys::STATUS),
        ("trustInfo", keys::TRUST_INFO),
        ("trustClaim", keys::TRUST_CLAIM),
    ] {
        set(&k, name, key.as_str());
    }
    set(&o, "keys", k);
    o
}

/// The payload ABI from `middleware::wasm_abi`:
/// `{version, importModule, maxSubscriptions, maxMessage, maxLog, fuelPerCall,
/// minTickMs, imports: [{name, params, returns}], exports: {memory, init, tick,
/// alloc, onMessage, onIrq}, errors: {name: code}, eventKinds: {Gpio, Timer,
/// DmaDone, UartRx}, dmaChannels: {Spi0, Uart1Tx}, edges: {Off, Rising,
/// Falling, Any}, maxEvents, maxTimers, minTimerUs, maxDma, spiBuses,
/// maxSpiKhz, uartPort, uartRxBuffer}`. `imports` lists every ABI revision's
/// imports (ABI 2 included); `eventKinds` and `dmaChannels` map names to the
/// `on_irq` `kind` and `source` codes.
#[wasm_bindgen]
pub fn abi() -> Object {
    let o = Object::new();
    set(&o, "version", wasm_abi::ABI_VERSION);
    set(&o, "importModule", wasm_abi::IMPORT_MODULE);
    set(&o, "maxSubscriptions", wasm_abi::MAX_SUBSCRIPTIONS as u32);
    set(&o, "maxMessage", wasm_abi::MAX_MESSAGE as u32);
    set(&o, "maxLog", wasm_abi::MAX_LOG as u32);
    // Exact in f64: well below 2^53.
    set(&o, "fuelPerCall", wasm_abi::FUEL_PER_CALL as f64);
    set(&o, "minTickMs", wasm_abi::MIN_TICK_MS);
    let imports: Array = wasm_abi::IMPORTS
        .iter()
        .map(|i| {
            let e = Object::new();
            set(&e, "name", i.name);
            set(&e, "params", i.params as u32);
            set(&e, "returns", i.returns);
            e
        })
        .collect();
    set(&o, "imports", imports);
    let ex = Object::new();
    set(&ex, "memory", wasm_abi::exports::MEMORY);
    set(&ex, "init", wasm_abi::exports::INIT);
    set(&ex, "tick", wasm_abi::exports::TICK);
    set(&ex, "alloc", wasm_abi::exports::ALLOC);
    set(&ex, "onMessage", wasm_abi::exports::ON_MESSAGE);
    set(&ex, "onIrq", wasm_abi::exports::ON_IRQ);
    set(&ex, "onRequest", wasm_abi::exports::ON_REQUEST);
    set(&o, "exports", ex);
    let errors = Object::new();
    for e in [
        wasm_abi::AbiError::InvalidArgument,
        wasm_abi::AbiError::OutOfBounds,
        wasm_abi::AbiError::NoResources,
        wasm_abi::AbiError::Unsupported,
        wasm_abi::AbiError::Transport,
        wasm_abi::AbiError::InvalidKey,
        wasm_abi::AbiError::Busy,
    ] {
        set(&errors, &format!("{e:?}"), e.code());
    }
    set(&o, "errors", errors);
    let kinds = Object::new();
    for k in [
        EventKind::Gpio,
        EventKind::Timer,
        EventKind::DmaDone,
        EventKind::UartRx,
    ] {
        set(&kinds, &format!("{k:?}"), k as i32);
    }
    set(&o, "eventKinds", kinds);
    let channels = Object::new();
    for c in [DmaChannel::Spi0, DmaChannel::Uart1Tx] {
        set(&channels, &format!("{c:?}"), c as i32);
    }
    set(&o, "dmaChannels", channels);
    let edges = Object::new();
    for e in [Edge::Off, Edge::Rising, Edge::Falling, Edge::Any] {
        set(&edges, &format!("{e:?}"), e as i32);
    }
    set(&o, "edges", edges);
    set(&o, "maxEvents", events::MAX_EVENTS as u32);
    set(&o, "maxTimers", periph::MAX_TIMERS as u32);
    set(&o, "minTimerUs", periph::MIN_TIMER_US);
    set(&o, "maxDma", periph::MAX_DMA as u32);
    set(&o, "spiBuses", periph::SPI_BUSES);
    set(&o, "maxSpiKhz", periph::MAX_SPI_KHZ);
    set(&o, "uartPort", periph::UART_PORT);
    set(&o, "uartRxBuffer", periph::UART_RX_BUFFER as u32);
    o
}

/// Whether the simulator was compiled in.
#[wasm_bindgen(js_name = hasSimulator)]
pub fn has_simulator() -> bool {
    cfg!(feature = "simulator")
}

fn pin_map(chip: &str) -> Result<&'static board_pins::PinMap, JsError> {
    board_pins::boards::by_chip(chip)
        .ok_or_else(|| JsError::new(&format!("no pin map for chip {chip:?}")))
}

fn get(o: &JsValue, key: &str) -> JsValue {
    Reflect::get(o, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

/// A JS number that is a whole number, as `i64`.
fn whole(v: &JsValue) -> Option<i64> {
    v.as_f64()
        .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
        .map(|f| f as i64)
}

/// The board's pins for the pin table: `{chip, spiRouting: "matrix" |
/// "rp-function-table", gpios: [{pin, reserved?: why, inputOnly, adc?:
/// channel, pwm?: {timer, channel}, i2c?: {bus, line: "sda"|"scl"}, spi?:
/// [{block, signal: "sck"|"mosi"|"miso"}]}], i2c: [{bus, sda, scl}], pwm:
/// [{timer, channel, pin}], adc: [pin], i2cBuses, spiBuses, i2cHz: [min, max], maxSpiKhz,
/// maxLabel, funcs: [name]}`. `chip` is a `sys/info` chip name; an unknown
/// one throws.
#[wasm_bindgen(js_name = boardPins)]
pub fn board_pins_info(chip: &str) -> Result<Object, JsError> {
    use board_pins::SpiRouting;
    use board_pins::rp::{SpiBlock, SpiSignal, spi_function};
    let map = pin_map(chip)?;
    let o = Object::new();
    set(&o, "chip", chip);
    let routing = match map.spi_routing() {
        SpiRouting::Matrix => "matrix",
        SpiRouting::RpFunctionTable => "rp-function-table",
    };
    set(&o, "spiRouting", routing);
    let wired: Vec<(u8, u8, u8)> = (0..2u8)
        .filter_map(|b| map.i2c_pins(b).map(|(d, c)| (b, d, c)))
        .collect();
    let gpios: Array = map
        .gpios()
        .map(|p| {
            let g = Object::new();
            set(&g, "pin", p);
            if map.is_reserved(p) {
                set(&g, "reserved", map.reason(p).unwrap_or("reserved"));
            }
            set(&g, "inputOnly", map.is_input_only(p));
            if let Ok(ch) = map.adc_channel(p) {
                set(&g, "adc", ch);
            }
            if let Some(((timer, channel), _)) = map.pwm_pins().find(|(_, q)| *q == p) {
                let w = Object::new();
                set(&w, "timer", timer);
                set(&w, "channel", channel);
                set(&g, "pwm", w);
            }
            if let Some(&(bus, sda, _)) = wired.iter().find(|(_, d, c)| *d == p || *c == p) {
                let i = Object::new();
                set(&i, "bus", bus);
                set(&i, "line", if sda == p { "sda" } else { "scl" });
                set(&g, "i2c", i);
            }
            if routing == "rp-function-table"
                && let Some((block, signal)) = spi_function(p)
            {
                let name = match signal {
                    SpiSignal::Sck => Some("sck"),
                    SpiSignal::Tx => Some("mosi"),
                    SpiSignal::Rx => Some("miso"),
                    SpiSignal::Csn => None,
                };
                if let Some(name) = name {
                    let s = Object::new();
                    set(&s, "block", if block == SpiBlock::Spi0 { 0 } else { 1 });
                    set(&s, "signal", name);
                    set(&g, "spi", s);
                }
            }
            g
        })
        .map(JsValue::from)
        .collect();
    set(&o, "gpios", gpios);
    let i2c: Array = wired
        .iter()
        .map(|&(bus, sda, scl)| {
            let b = Object::new();
            set(&b, "bus", bus);
            set(&b, "sda", sda);
            set(&b, "scl", scl);
            JsValue::from(b)
        })
        .collect();
    set(&o, "i2c", i2c);
    let pwm: Array = map
        .pwm_pins()
        .map(|((timer, channel), pin)| {
            let b = Object::new();
            set(&b, "timer", timer);
            set(&b, "channel", channel);
            set(&b, "pin", pin);
            JsValue::from(b)
        })
        .collect();
    set(&o, "pwm", pwm);
    let adc: Array = map.adc_pins().iter().map(|&p| JsValue::from(p)).collect();
    set(&o, "adc", adc);
    set(&o, "i2cBuses", periph::I2C_BUSES);
    set(&o, "spiBuses", periph::SPI_BUSES);
    let hz = Array::of2(
        &JsValue::from(*periph::I2C_FREQ_HZ.start()),
        &JsValue::from(*periph::I2C_FREQ_HZ.end()),
    );
    set(&o, "i2cHz", hz);
    set(&o, "maxSpiKhz", periph::MAX_SPI_KHZ);
    set(&o, "maxLabel", crate::pins::MAX_LABEL as u32);
    let funcs: Array = crate::pins::Func::ALL
        .iter()
        .map(|f| JsValue::from_str(f.name()))
        .collect();
    set(&o, "funcs", funcs);
    Ok(o)
}

/// Checks a pin table on `chip` (see `crate::pins`). `rows` is
/// `[{label, pin: number|null, func, bus}]`, `settings` is
/// `[{kind: "i2c", bus, hz} | {kind: "spi", bus, khz, mode}]`. Returns
/// `[{row: number|null, field: "label"|"pin"|"func"|"bus"|"setting"|"table",
/// message, fix?: {kind: "pin", pin} | {kind: "add", func, bus, pin?}}]`.
#[wasm_bindgen(js_name = checkPins)]
pub fn check_pins(chip: &str, rows: Array, settings: Array) -> Result<Array, JsError> {
    use crate::pins::{BusSetting, Fix, Func, Row, check};
    let map = pin_map(chip)?;
    let rows: Vec<Row> = rows
        .iter()
        .map(|r| {
            let func = get(&r, "func").as_string().unwrap_or_default();
            Row {
                label: get(&r, "label").as_string().unwrap_or_default(),
                pin: whole(&get(&r, "pin")),
                func: Func::parse(&func).ok_or(func),
                bus: whole(&get(&r, "bus")).unwrap_or(0),
            }
        })
        .collect();
    let settings: Vec<BusSetting> = settings
        .iter()
        .filter_map(|s| {
            let bus = whole(&get(&s, "bus"))?;
            match get(&s, "kind").as_string().as_deref() {
                Some("i2c") => Some(BusSetting::I2c {
                    bus,
                    hz: whole(&get(&s, "hz"))?,
                }),
                Some("spi") => Some(BusSetting::Spi {
                    bus,
                    khz: whole(&get(&s, "khz"))?,
                    mode: whole(&get(&s, "mode"))?,
                }),
                _ => None,
            }
        })
        .collect();
    Ok(check(map, &rows, &settings)
        .into_iter()
        .map(|i| {
            let o = Object::new();
            set(
                &o,
                "row",
                i.row.map_or(JsValue::NULL, |r| JsValue::from(r as u32)),
            );
            set(&o, "field", i.field.name());
            set(&o, "message", i.message.as_str());
            if let Some(fix) = i.fix {
                let f = Object::new();
                match fix {
                    Fix::Pin(p) => {
                        set(&f, "kind", "pin");
                        set(&f, "pin", p);
                    }
                    Fix::AddRow { func, bus, pin } => {
                        set(&f, "kind", "add");
                        set(&f, "func", func.name());
                        set(&f, "bus", bus);
                        set(&f, "pin", pin.map_or(JsValue::NULL, JsValue::from));
                    }
                }
                set(&o, "fix", f);
            }
            JsValue::from(o)
        })
        .collect())
}

/// The array at `o[key]`, or an empty one.
fn array(o: &JsValue, key: &str) -> Array {
    let v = get(o, key);
    if Array::is_array(&v) {
        Array::from(&v)
    } else {
        Array::new()
    }
}

/// The text at `o[key]`, or empty.
fn text(o: &JsValue, key: &str) -> String {
    get(o, key).as_string().unwrap_or_default()
}

/// A guard `Expr` object. Recursion is bounded by the constant
/// `MAX_GUARD_DEPTH + 1`: a subtree below that is not read, it becomes a
/// leaf there and the check reports the guard as too deep.
fn machine_expr(v: &JsValue, depth: u8, at: &str) -> Result<crate::machines::Expr, JsError> {
    use crate::machines::Expr;
    use state_table::{CmpOp, MAX_GUARD_DEPTH};
    if depth > MAX_GUARD_DEPTH {
        return Ok(Expr::Const(0));
    }
    let sub = |key: &str| -> Result<Box<Expr>, JsError> {
        machine_expr(&get(v, key), depth + 1, &format!("{at}.{key}")).map(Box::new)
    };
    match text(v, "kind").as_str() {
        "pin" => Ok(Expr::Pin(text(v, "label"))),
        "state" => Ok(Expr::State(text(v, "machine"), text(v, "state"))),
        "const" => whole(&get(v, "value"))
            .and_then(|n| i32::try_from(n).ok())
            .map(Expr::Const)
            .ok_or_else(|| JsError::new(&format!("{at}.value must be a whole 32-bit number"))),
        "cmp" => {
            let op = CmpOp::parse(&text(v, "op")).ok_or_else(|| {
                JsError::new(&format!("{at}.op must be eq, ne, lt, le, gt or ge"))
            })?;
            Ok(Expr::Cmp(op, sub("a")?, sub("b")?))
        }
        "and" => Ok(Expr::And(sub("a")?, sub("b")?)),
        "or" => Ok(Expr::Or(sub("a")?, sub("b")?)),
        "not" => Ok(Expr::Not(sub("a")?)),
        other => Err(JsError::new(&format!(
            "{at}.kind {other:?} is not an expression"
        ))),
    }
}

/// An `ActionRef` list.
fn machine_actions(list: Array, at: &str) -> Result<Vec<crate::machines::ActionIn>, JsError> {
    use crate::machines::ActionIn;
    list.iter()
        .enumerate()
        .map(|(k, a)| match text(&a, "kind").as_str() {
            "write" => {
                let level = get(&a, "level");
                Ok(ActionIn::Write {
                    label: text(&a, "label"),
                    level: level.as_f64() == Some(1.0) || level.as_bool() == Some(true),
                })
            }
            "toggle" => Ok(ActionIn::Toggle {
                label: text(&a, "label"),
            }),
            "publish" => Ok(ActionIn::Publish {
                key: text(&a, "key"),
                text: text(&a, "text"),
            }),
            "log" => Ok(ActionIn::Log {
                text: text(&a, "text"),
            }),
            "raise" => Ok(ActionIn::Raise {
                machine: get(&a, "machine").as_string(),
                event: text(&a, "event"),
            }),
            other => Err(JsError::new(&format!(
                "{at}[{k}].kind {other:?} is not an action"
            ))),
        })
        .collect()
}

/// The pin table's `[{label, func}]` as the checks read it.
fn pins_in(pins: Array) -> Vec<crate::machines::PinIn> {
    use crate::machines::PinIn;
    use crate::pins::Func;
    pins.iter()
        .map(|p| {
            let func = text(&p, "func");
            PinIn {
                label: text(&p, "label"),
                func: Func::parse(&func).ok_or(func),
            }
        })
        .collect()
}

/// The page's machines (see `checkMachines`) as the checks read them.
fn machines_in(machines: Array) -> Result<Vec<crate::machines::MachineIn>, JsError> {
    use crate::machines::{MachineIn, RowIn, StateIn, flatten};
    machines
        .iter()
        .enumerate()
        .map(|(mi, m)| {
            let at = format!("machines[{mi}]");
            let states = array(&m, "states")
                .iter()
                .enumerate()
                .map(|(si, s)| {
                    Ok(StateIn {
                        name: text(&s, "name"),
                        entry: machine_actions(
                            array(&s, "entry"),
                            &format!("{at}.states[{si}].entry"),
                        )?,
                    })
                })
                .collect::<Result<Vec<_>, JsError>>()?;
            let rows = array(&m, "rows")
                .iter()
                .enumerate()
                .map(|(ri, r)| {
                    let rat = format!("{at}.rows[{ri}]");
                    let g = get(&r, "guard");
                    let guard = if g.is_null() || g.is_undefined() {
                        None
                    } else {
                        Some(flatten(&machine_expr(&g, 1, &format!("{rat}.guard"))?))
                    };
                    Ok(RowIn {
                        state: text(&r, "state"),
                        event: text(&r, "event"),
                        guard,
                        next: text(&r, "next"),
                        actions: machine_actions(array(&r, "actions"), &format!("{rat}.actions"))?,
                    })
                })
                .collect::<Result<Vec<_>, JsError>>()?;
            Ok(MachineIn {
                id: text(&m, "id"),
                initial: text(&m, "initial"),
                states,
                events: array(&m, "events")
                    .iter()
                    .map(|e| text(&e, "name"))
                    .collect(),
                rows,
            })
        })
        .collect()
}

/// A table or flow fix as `{kind: "add-state"|"add-event"|"set-initial",
/// name, machine?} | {kind: "add-pin", label, func}` (`machine`: the index
/// of the machine an event is added to, when it is not the issue's own).
fn fix_object(fix: crate::machines::MachineFix) -> Object {
    use crate::machines::MachineFix;
    let f = Object::new();
    match fix {
        MachineFix::AddState(name) => {
            set(&f, "kind", "add-state");
            set(&f, "name", name);
        }
        MachineFix::AddEvent(name) => {
            set(&f, "kind", "add-event");
            set(&f, "name", name);
        }
        MachineFix::AddEventTo { machine, name } => {
            set(&f, "kind", "add-event");
            set(&f, "name", name);
            set(&f, "machine", machine as u32);
        }
        MachineFix::SetInitial(name) => {
            set(&f, "kind", "set-initial");
            set(&f, "name", name);
        }
        MachineFix::AddPin { label, func } => {
            set(&f, "kind", "add-pin");
            set(&f, "label", label);
            set(&f, "func", func.name());
        }
    }
    f
}

fn index_or_null(i: Option<usize>) -> JsValue {
    i.map_or(JsValue::NULL, |r| JsValue::from(r as u32))
}

/// Checks state machines against the pin table (see `crate::machines` and
/// the `state-table` crate). `machines` is `[{id, name, initial, states:
/// [{name, note, entry: ActionRef[]}], events: [{name, note}], rows:
/// [{state, event, guard: Expr|null, next, actions: ActionRef[]}]}]` with
/// `Expr` `{kind: "pin", label} | {kind: "const", value} | {kind: "state",
/// machine, state} | {kind: "cmp", op, a, b} | {kind: "and"|"or", a, b} |
/// {kind: "not", a}` and `ActionRef` `{kind: "write", label, level: 0|1} |
/// {kind: "toggle", label} | {kind: "publish", key, text} | {kind: "log",
/// text} | {kind: "raise", event, machine?}` (`web/src/model/machines.ts`).
/// `pins` is `[{label, func}]` with `func` a pin-table function name
/// (`"gpio-out"`, `"gpio-in"`, ...). Expressions nested deeper than the
/// limit are reported as issues, not read; an unknown expression or action
/// kind, or a non-integer constant, throws. Returns `[{machine, list:
/// "machine"|"states"|"events"|"rows", row: number|null, field, severity:
/// "error"|"warning", message, fix?}]` with `fix` as `fix_object` makes.
#[wasm_bindgen(js_name = checkMachines)]
pub fn check_machines(machines: Array, pins: Array) -> Result<Array, JsError> {
    let pins = pins_in(pins);
    let machines = machines_in(machines)?;
    Ok(crate::machines::check(&machines, &pins)
        .into_iter()
        .map(|i| {
            let o = Object::new();
            set(&o, "machine", i.machine as u32);
            set(&o, "list", i.list);
            set(&o, "row", index_or_null(i.row));
            set(&o, "field", i.field);
            set(&o, "severity", i.severity);
            set(&o, "message", i.message.as_str());
            if let Some(fix) = i.fix {
                set(&o, "fix", fix_object(fix));
            }
            JsValue::from(o)
        })
        .collect())
}

/// A flow node's kind and `props` (`web/src/model/flows.ts`).
fn flow_kind(n: &JsValue, at: &str) -> Result<crate::flows::KindIn, JsError> {
    use crate::flows::{EntryIn, KindIn, split_ref};
    use crate::machines::flatten;
    use state_table::flow::Edge;
    let props = get(n, "props");
    let number = |key: &str| whole(&get(&props, key)).unwrap_or(0);
    Ok(match text(n, "kind").as_str() {
        "entry" => KindIn::Entry(match text(&props, "on").as_str() {
            "init" => EntryIn::Init,
            "every" => EntryIn::Every { ms: number("ms") },
            "message" => EntryIn::Message {
                key: text(&props, "key"),
            },
            "pin-edge" => EntryIn::PinEdge {
                label: text(&props, "label"),
                edge: match text(&props, "edge").as_str() {
                    "rise" => Edge::Rise,
                    "fall" => Edge::Fall,
                    _ => Edge::Both,
                },
            },
            "entered" => {
                let (machine, state) = split_ref(&text(&props, "state"));
                EntryIn::Entered { machine, state }
            }
            "phase" => {
                let (em, phase) = split_ref(&text(&props, "phase"));
                let every = get(&props, "every");
                EntryIn::Phase {
                    em,
                    phase,
                    state: text(&props, "state"),
                    every: if every.is_undefined() || every.is_null() {
                        None
                    } else {
                        Some(whole(&every).unwrap_or(0))
                    },
                }
            }
            other => {
                return Err(JsError::new(&format!(
                    "{at}.props.on {other:?} is not an entry"
                )));
            }
        }),
        "action" if text(&props, "kind").starts_with("phase-") => {
            KindIn::Phase(flow_phase_action(&props, &format!("{at}.props"))?)
        }
        "action" => {
            let list = Array::of1(&props);
            let mut acts = machine_actions(list, &format!("{at}.props"))?;
            KindIn::Action(
                acts.pop()
                    .ok_or_else(|| JsError::new(&format!("{at}.props is not an action")))?,
            )
        }
        "branch" => KindIn::Branch(flatten(&machine_expr(
            &get(&props, "cond"),
            1,
            &format!("{at}.props.cond"),
        )?)),
        "repeat" => KindIn::Repeat {
            times: number("times"),
        },
        "sequence" => KindIn::Sequence {
            steps: number("steps"),
        },
        other => {
            return Err(JsError::new(&format!(
                "{at}.kind {other:?} is not a node kind"
            )));
        }
    })
}

/// A `phase-done` or `phase-report` action's props (`web/src/model/flows.ts`).
fn flow_phase_action(props: &JsValue, at: &str) -> Result<crate::flows::PhaseIn, JsError> {
    use crate::flows::{AfterIn, PhaseIn, ValueIn, split_ref};
    let (em, phase) = split_ref(&text(props, "phase"));
    match text(props, "kind").as_str() {
        "phase-done" => {
            let after = get(props, "after");
            let after = if after.is_undefined() || after.is_null() {
                AfterIn::Now
            } else if let Some(p) = after.as_string() {
                AfterIn::Param(p)
            } else {
                AfterIn::Count(whole(&after).ok_or_else(|| {
                    JsError::new(&format!("{at}.after must be a whole number or a parameter"))
                })?)
            };
            Ok(PhaseIn::Done { em, phase, after })
        }
        "phase-report" => {
            let values = array(props, "values")
                .iter()
                .enumerate()
                .map(|(i, v)| match text(&v, "kind").as_str() {
                    "const" => whole(&get(&v, "value")).map(ValueIn::Const).ok_or_else(|| {
                        JsError::new(&format!("{at}.values[{i}].value must be a whole number"))
                    }),
                    "pin" => Ok(ValueIn::Pin(text(&v, "label"))),
                    "param" => Ok(ValueIn::Param(text(&v, "param"))),
                    "runs" => Ok(ValueIn::Runs),
                    other => Err(JsError::new(&format!(
                        "{at}.values[{i}].kind {other:?} is not a report value"
                    ))),
                })
                .collect::<Result<_, _>>()?;
            Ok(PhaseIn::Report { em, phase, values })
        }
        other => Err(JsError::new(&format!(
            "{at}.kind {other:?} is not a phase action"
        ))),
    }
}

/// Checks flows against the machines and the pin table (see
/// `crate::flows` and `state_table::flow`). `flows` is `[{kind: "flow",
/// id, name, nodes: [{id, kind, x, y, props}], wires: [{from: {node,
/// port}, to: {node, port}}]}]` (`web/src/model/flows.ts`); `machines` and
/// `pins` as for `checkMachines`; `phases` (optional) the unit's
/// equipment phases `[{em, phase, params: [id]}]` that phase entries
/// (`{on: "phase", phase: "<em>.<phase>", state, every?}`) and phase
/// actions (`{kind: "phase-done", phase, after?: number | param}`,
/// `{kind: "phase-report", phase, values: [{kind: "const", value} |
/// {kind: "pin", label} | {kind: "param", param} | {kind: "runs"}]}`)
/// name. An unknown node kind, entry or action throws. Returns `[{flow,
/// node: number|null, wire: number|null, severity, message, fix?}]` (node
/// and wire are indices).
#[wasm_bindgen(js_name = checkFlows)]
pub fn check_flows(
    flows: Array,
    machines: Array,
    pins: Array,
    phases: Option<Array>,
) -> Result<Array, JsError> {
    use crate::flows::{FlowIn, NodeOwned, PhaseDeclIn, WireIn, check_in};
    let phases: Vec<PhaseDeclIn> = phases
        .map(|a| {
            a.iter()
                .map(|p| PhaseDeclIn {
                    em: text(&p, "em"),
                    phase: text(&p, "phase"),
                    params: array(&p, "params")
                        .iter()
                        .map(|v| v.as_string().unwrap_or_default())
                        .collect(),
                })
                .collect()
        })
        .unwrap_or_default();
    let pins = pins_in(pins);
    let machines = machines_in(machines)?;
    let flows: Vec<FlowIn> = flows
        .iter()
        .enumerate()
        .map(|(fi, f)| {
            let nodes = array(&f, "nodes")
                .iter()
                .enumerate()
                .map(|(ni, n)| {
                    Ok(NodeOwned {
                        id: text(&n, "id"),
                        kind: flow_kind(&n, &format!("flows[{fi}].nodes[{ni}]"))?,
                    })
                })
                .collect::<Result<Vec<_>, JsError>>()?;
            let wires = array(&f, "wires")
                .iter()
                .map(|w| {
                    let (from, to) = (get(&w, "from"), get(&w, "to"));
                    WireIn {
                        from: text(&from, "node"),
                        from_port: text(&from, "port"),
                        to: text(&to, "node"),
                        to_port: text(&to, "port"),
                    }
                })
                .collect();
            Ok(FlowIn {
                id: text(&f, "id"),
                nodes,
                wires,
            })
        })
        .collect::<Result<_, JsError>>()?;
    Ok(check_in(&flows, &machines, &pins, &phases)
        .into_iter()
        .map(|i| {
            let o = Object::new();
            set(&o, "flow", i.flow as u32);
            set(&o, "node", index_or_null(i.node));
            set(&o, "wire", index_or_null(i.wire));
            set(&o, "severity", i.severity);
            set(&o, "message", i.message.as_str());
            if let Some(fix) = i.fix {
                set(&o, "fix", fix_object(fix));
            }
            JsValue::from(o)
        })
        .collect())
}

/// `{id, name?, params: [{id, min, max}]}` phase classes
/// (`web/src/model/procedural.ts`) as the recipe checks read them.
fn classes_in(classes: &Array) -> Vec<crate::recipes::ClassIn> {
    use crate::recipes::{ClassIn, ParamIn};
    classes
        .iter()
        .map(|c| ClassIn {
            name: text(&c, "id"),
            params: array(&c, "params")
                .iter()
                .map(|p| ParamIn {
                    name: text(&p, "id"),
                    min: whole(&get(&p, "min"))
                        .and_then(|v| i32::try_from(v).ok())
                        .unwrap_or(0),
                    max: whole(&get(&p, "max"))
                        .and_then(|v| i32::try_from(v).ok())
                        .unwrap_or(-1),
                })
                .collect(),
        })
        .collect()
}

/// A master recipe (`web/src/model/recipe.ts`): `{id, unitProcedures:
/// [{id, unitClass: string|null}], steps: [{id, unitProcedure, class,
/// params: {name: number}}], transitions: [{from: [id], to: [id]}]}`. A
/// parameter value that is not a whole `i32` throws.
fn recipe_in(r: &JsValue) -> Result<crate::recipes::RecipeIn, JsError> {
    use crate::recipes::{RecipeIn, StepIn, TransitionIn, UnitProcIn};
    let ids = |v: JsValue| -> Vec<String> {
        if Array::is_array(&v) {
            Array::from(&v)
                .iter()
                .filter_map(|x| x.as_string())
                .collect()
        } else {
            Vec::new()
        }
    };
    let steps = array(r, "steps")
        .iter()
        .enumerate()
        .map(|(si, s)| {
            let params = get(&s, "params");
            let args = if params.is_object() {
                Object::entries(&Object::from(params))
                    .iter()
                    .map(|e| {
                        let pair = Array::from(&e);
                        let name = pair.get(0).as_string().unwrap_or_default();
                        let value = whole(&pair.get(1))
                            .and_then(|v| i32::try_from(v).ok())
                            .ok_or_else(|| {
                                JsError::new(&format!(
                                    "steps[{si}].params.{name} must be a whole number"
                                ))
                            })?;
                        Ok((name, value))
                    })
                    .collect::<Result<Vec<_>, JsError>>()?
            } else {
                Vec::new()
            };
            Ok(StepIn {
                id: text(&s, "id"),
                unit_proc: text(&s, "unitProcedure"),
                class: text(&s, "class"),
                args,
            })
        })
        .collect::<Result<Vec<_>, JsError>>()?;
    Ok(RecipeIn {
        id: text(r, "id"),
        unit_procs: array(r, "unitProcedures")
            .iter()
            .map(|u| UnitProcIn {
                id: text(&u, "id"),
                unit_class: get(&u, "unitClass").as_string(),
            })
            .collect(),
        steps,
        transitions: array(r, "transitions")
            .iter()
            .map(|t| TransitionIn {
                from: ids(get(&t, "from")),
                to: ids(get(&t, "to")),
            })
            .collect(),
    })
}

/// Allocation units (`web/src/model/plant.ts` `allocationUnits`): `[{id,
/// class, online, phases: [{em, name, class, ranges: [[min, max]], runMs?}]}]`.
fn units_in(units: &Array) -> Vec<crate::recipes::UnitIn> {
    use crate::recipes::{UnitIn, UnitPhaseIn};
    let int = |v: JsValue| whole(&v).and_then(|v| i32::try_from(v).ok());
    units
        .iter()
        .map(|u| UnitIn {
            id: text(&u, "id"),
            class: text(&u, "class"),
            online: get(&u, "online").as_bool().unwrap_or(false),
            phases: array(&u, "phases")
                .iter()
                .map(|p| UnitPhaseIn {
                    em: text(&p, "em"),
                    name: text(&p, "name"),
                    class: text(&p, "class"),
                    ranges: array(&p, "ranges")
                        .iter()
                        .map(|r| {
                            let r = Array::from(&r);
                            (int(r.get(0)).unwrap_or(1), int(r.get(1)).unwrap_or(0))
                        })
                        .collect(),
                    run_ms: whole(&get(&p, "runMs"))
                        .and_then(|v| u32::try_from(v).ok())
                        .unwrap_or(100),
                })
                .collect(),
        })
        .collect()
}

fn binding_object(b: &crate::recipes::Binding) -> JsValue {
    let o = Object::new();
    set(&o, "step", b.step.as_str());
    set(&o, "unit", b.unit.as_str());
    set(&o, "em", b.em.as_str());
    set(&o, "phase", b.phase.as_str());
    JsValue::from(o)
}

fn alloc_failure_object(f: &crate::recipes::AllocFailure) -> JsValue {
    let o = Object::new();
    set(&o, "ok", false);
    set(&o, "message", f.message.as_str());
    set(
        &o,
        "unitProcedure",
        f.unit_proc
            .as_deref()
            .map_or(JsValue::NULL, JsValue::from_str),
    );
    let reasons: Array = f
        .reasons
        .iter()
        .map(|(unit, why)| {
            let r = Object::new();
            set(&r, "unit", unit.as_str());
            set(&r, "reason", why.as_str());
            JsValue::from(r)
        })
        .collect();
    set(&o, "reasons", reasons);
    JsValue::from(o)
}

/// Checks phase classes and a master recipe (`recipe-core`'s rules).
/// Returns `[{at: "recipe"|"class"|"unitProc"|"step"|"transition", index:
/// number|null, sub: number|null, severity: "error"|"warning", message,
/// fix?}]` (`fix`: see `recipe_fix_object`).
#[wasm_bindgen(js_name = checkRecipe)]
pub fn check_recipe(recipe: JsValue, classes: Array) -> Result<Array, JsError> {
    let r = recipe_in(&recipe)?;
    let c = classes_in(&classes);
    Ok(crate::recipes::check(&r, &c)
        .into_iter()
        .map(|i| {
            let o = Object::new();
            set(&o, "at", i.at);
            set(&o, "index", index_or_null(i.index));
            set(&o, "sub", index_or_null(i.sub));
            set(&o, "severity", i.severity);
            set(&o, "message", i.message.as_str());
            if let Some(f) = &i.fix {
                set(&o, "fix", recipe_fix_object(f));
            }
            JsValue::from(o)
        })
        .collect())
}

/// `{kind: "setArg", step, param, value}`, `{kind: "removeArg", step,
/// param}`, `{kind: "removeTransition", transition}`, `{kind:
/// "removeFromTransition", transition, side, step}` (a step id) or
/// `{kind: "removeUnitProc", unitProc}`.
fn recipe_fix_object(f: &crate::recipes::RecipeFix) -> Object {
    use crate::recipes::RecipeFix;
    let o = Object::new();
    match f {
        RecipeFix::SetArg { step, param, value } => {
            set(&o, "kind", "setArg");
            set(&o, "step", *step as u32);
            set(&o, "param", param.as_str());
            set(&o, "value", *value);
        }
        RecipeFix::RemoveArg { step, param } => {
            set(&o, "kind", "removeArg");
            set(&o, "step", *step as u32);
            set(&o, "param", param.as_str());
        }
        RecipeFix::RemoveTransition(t) => {
            set(&o, "kind", "removeTransition");
            set(&o, "transition", *t as u32);
        }
        RecipeFix::RemoveFromTransition {
            transition,
            side,
            step,
        } => {
            set(&o, "kind", "removeFromTransition");
            set(&o, "transition", *transition as u32);
            set(&o, "side", *side);
            set(&o, "step", step.as_str());
        }
        RecipeFix::RemoveUnitProc(u) => {
            set(&o, "kind", "removeUnitProc");
            set(&o, "unitProc", *u as u32);
        }
    }
    o
}

/// Binds every step of `recipe` to an equipment phase of `units`. Returns
/// `{ok: true, bindings: [{step, unit, em, phase}]}` or `{ok: false,
/// message, unitProcedure: string|null, reasons: [{unit, reason}]}`.
#[wasm_bindgen(js_name = allocateRecipe)]
pub fn allocate_recipe(recipe: JsValue, classes: Array, units: Array) -> Result<JsValue, JsError> {
    let r = recipe_in(&recipe)?;
    let c = classes_in(&classes);
    let u = units_in(&units);
    Ok(match crate::recipes::allocate(&r, &c, &u) {
        Ok(bindings) => {
            let o = Object::new();
            set(&o, "ok", true);
            set(
                &o,
                "bindings",
                bindings.iter().map(binding_object).collect::<Array>(),
            );
            JsValue::from(o)
        }
        Err(f) => alloc_failure_object(&f),
    })
}

/// Allocates `recipe` and runs it against simulated units until
/// `untilMs`, following `script: [{at, kind: "start"|"hold"|"restart"|
/// "stop"|"abort"|"reset"|"lose"|"find", unit?}]` (`unit`: a unit id, for
/// lose and find). Returns `{ok: true, state, bindings, lines: [{t, kind,
/// text}]}` or an allocation failure as `allocateRecipe`. An unknown
/// script kind throws.
#[wasm_bindgen(js_name = simulateRecipe)]
pub fn simulate_recipe(
    recipe: JsValue,
    classes: Array,
    units: Array,
    script: Array,
    until_ms: u32,
) -> Result<JsValue, JsError> {
    use crate::recipes::{ScriptEventIn, command_named};
    let r = recipe_in(&recipe)?;
    let c = classes_in(&classes);
    let u = units_in(&units);
    let script = script
        .iter()
        .map(|s| {
            let at = whole(&get(&s, "at"))
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(0);
            let kind = text(&s, "kind");
            let event = match kind.as_str() {
                "lose" => ScriptEventIn::Lose(text(&s, "unit")),
                "find" => ScriptEventIn::Find(text(&s, "unit")),
                other => ScriptEventIn::Operator(command_named(other).ok_or_else(|| {
                    JsError::new(&format!(
                        "script kind {other:?} is not a command, lose or find"
                    ))
                })?),
            };
            Ok((at, event))
        })
        .collect::<Result<Vec<_>, JsError>>()?;
    Ok(
        match crate::recipes::simulate(&r, &c, &u, &script, u64::from(until_ms)) {
            Ok(t) => {
                let o = Object::new();
                set(&o, "ok", true);
                set(&o, "state", t.state);
                set(
                    &o,
                    "bindings",
                    t.bindings.iter().map(binding_object).collect::<Array>(),
                );
                let lines: Array = t
                    .lines
                    .iter()
                    .map(|l| {
                        let x = Object::new();
                        set(&x, "t", l.t as f64);
                        set(&x, "kind", l.kind);
                        set(&x, "text", l.text.as_str());
                        JsValue::from(x)
                    })
                    .collect();
                set(&o, "lines", lines);
                JsValue::from(o)
            }
            Err(f) => alloc_failure_object(&f),
        },
    )
}

/// 16 lower- or upper-case hex digits to a node id.
fn parse_node(hex: &str) -> Option<middleware::SenderId> {
    let b = hex.as_bytes();
    if b.len() != 16 {
        return None;
    }
    let mut id = [0u8; 8];
    for (i, out) in id.iter_mut().enumerate() {
        let pair = core::str::from_utf8(b.get(2 * i..2 * i + 2)?).ok()?;
        *out = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(middleware::SenderId(id))
}

/// A node id as 16 lower-case hex digits.
fn node_hex(id: middleware::SenderId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// A control recipe run against payloads the page drives
/// (`crate::recipes::run`): recipe-core's engine whose commands come out
/// as `{kind: "send", id, unit, key, body, timeoutMs, command, step}` for
/// the page to hand to the unit serving `key`, and whose other effects
/// are `{kind: "trace", t, type, text}` lines.
#[wasm_bindgen]
pub struct RecipeRun {
    run: Option<crate::recipes::run::Run>,
}

fn run_effect_object(e: &crate::recipes::run::RunEffect) -> JsValue {
    use crate::recipes::run::RunEffect;
    let o = Object::new();
    match e {
        RunEffect::Send(s) => {
            set(&o, "kind", "send");
            set(&o, "id", s.id);
            set(&o, "unit", s.unit.as_str());
            set(&o, "key", s.key.as_str());
            set(&o, "body", bytes(&s.body));
            set(&o, "timeoutMs", s.timeout_ms);
            set(&o, "command", s.command);
            set(&o, "step", s.step.as_str());
        }
        RunEffect::Line(l) => {
            set(&o, "kind", "trace");
            set(&o, "t", l.t as f64);
            set(&o, "type", l.kind);
            set(&o, "text", l.text.as_str());
        }
    }
    JsValue::from(o)
}

#[wasm_bindgen]
impl RecipeRun {
    /// Compiles and allocates `recipe` on `units` (as `allocateRecipe`,
    /// each unit also with `node`: 16 hex digits) and builds an Idle
    /// engine; `config` is `{ackTimeoutMs, settleTimeoutMs, maxCommands}`
    /// (recipe-core's defaults for those left out). Throws with the
    /// allocation failure's message, or for a missing node id.
    #[wasm_bindgen(constructor)]
    pub fn new(
        recipe: JsValue,
        classes: Array,
        units: Array,
        config: JsValue,
    ) -> Result<RecipeRun, JsError> {
        use recipe_core::engine::Config;
        let r = recipe_in(&recipe)?;
        let c = classes_in(&classes);
        let u = units_in(&units);
        let nodes = units
            .iter()
            .map(|x| parse_node(&text(&x, "node")).map(|n| n.0))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| js_err("every unit needs a node id (16 hex digits)"))?;
        let num = |k: &str| whole(&get(&config, k)).and_then(|v| u32::try_from(v).ok());
        let cfg = Config {
            ack_timeout_ms: num("ackTimeoutMs").unwrap_or(Config::DEFAULT.ack_timeout_ms),
            settle_timeout_ms: num("settleTimeoutMs").unwrap_or(Config::DEFAULT.settle_timeout_ms),
            max_commands: num("maxCommands")
                .and_then(|v| u8::try_from(v).ok())
                .unwrap_or(Config::DEFAULT.max_commands),
        };
        let run = crate::recipes::run::Run::new(r, c, u, nodes, cfg).map_err(|e| match e {
            crate::recipes::run::RunError::Alloc(f) => js_err(&f.message),
            crate::recipes::run::RunError::Nodes => js_err("one node id per unit"),
        })?;
        Ok(RecipeRun { run: Some(run) })
    }

    fn get(&self) -> Result<&crate::recipes::run::Run, JsError> {
        self.run
            .as_ref()
            .ok_or_else(|| js_err("the run failed earlier"))
    }

    /// One input at `nowMs`: `{kind: "operator", command}` (`"start"`,
    /// ...), `{kind: "reply", id, ok, reply?, error?}` (what the
    /// simulator's `request` returned), `{kind: "report", key, data}` (a
    /// publication), `{kind: "lost"|"found", unit}` or `{kind: "tick"}`.
    /// Returns the effects in order. Throws for an unknown kind or
    /// command.
    pub fn on(&mut self, now_ms: f64, input: JsValue) -> Result<Array, JsError> {
        use crate::recipes::run::RunInput;
        let kind = text(&input, "kind");
        let i = match kind.as_str() {
            "operator" => {
                let c = text(&input, "command");
                RunInput::Operator(
                    crate::recipes::command_named(&c)
                        .ok_or_else(|| js_err(&format!("no command {c:?}")))?,
                )
            }
            "reply" => RunInput::Reply {
                id: whole(&get(&input, "id"))
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0),
                result: if get(&input, "ok").as_bool().unwrap_or(false) {
                    Ok(Uint8Array::new(&get(&input, "reply")).to_vec())
                } else {
                    Err(text(&input, "error"))
                },
            },
            "report" => RunInput::Report {
                key: text(&input, "key"),
                body: Uint8Array::new(&get(&input, "data")).to_vec(),
            },
            "lost" => RunInput::Lost(text(&input, "unit")),
            "found" => RunInput::Found(text(&input, "unit")),
            "tick" => RunInput::Tick,
            other => return Err(js_err(&format!("no input kind {other:?}"))),
        };
        self.get()?;
        let run = self
            .run
            .take()
            .ok_or_else(|| js_err("the run failed earlier"))?;
        let now = if now_ms.is_finite() && now_ms > 0.0 {
            now_ms as u64
        } else {
            0
        };
        let (run, fx) = run.on(now, i);
        self.run = Some(run);
        Ok(fx.iter().map(run_effect_object).collect())
    }

    /// The procedure's state name (`"Idle"`, `"Running"`, ...).
    #[wasm_bindgen(getter)]
    pub fn state(&self) -> Result<String, JsError> {
        Ok(self.get()?.state().to_string())
    }

    /// `[{id, status: "inactive"|"active"|"started"|"done"}]` in step
    /// order.
    pub fn steps(&self) -> Result<Array, JsError> {
        Ok(self
            .get()?
            .steps()
            .into_iter()
            .map(|(id, status)| {
                let o = Object::new();
                set(&o, "id", id.as_str());
                set(&o, "status", status);
                JsValue::from(o)
            })
            .collect())
    }

    /// `[{unit, em, phase, state: name|null, values: [number]}]` for the
    /// bound phases.
    pub fn phases(&self) -> Result<Array, JsError> {
        Ok(self
            .get()?
            .phases()
            .into_iter()
            .map(|p| {
                let o = Object::new();
                set(&o, "unit", p.unit.as_str());
                set(&o, "em", p.em.as_str());
                set(&o, "phase", p.phase.as_str());
                set(
                    &o,
                    "state",
                    p.state.map_or(JsValue::NULL, JsValue::from_str),
                );
                set(
                    &o,
                    "values",
                    p.values
                        .iter()
                        .map(|v| JsValue::from(*v))
                        .collect::<Array>(),
                );
                JsValue::from(o)
            })
            .collect())
    }

    /// `[{step, unit, em, phase}]`.
    pub fn bindings(&self) -> Result<Array, JsError> {
        Ok(self.get()?.bindings().iter().map(binding_object).collect())
    }

    /// When the engine next needs a tick (ms), or `undefined`.
    #[wasm_bindgen(js_name = nextDeadline)]
    pub fn next_deadline(&self) -> Result<Option<f64>, JsError> {
        Ok(self.get()?.next_deadline().map(|t| t as f64))
    }
}

/// The S88 phase state model: `{states: [name], commands: [name], next:
/// [[name|null]]}` with `next[state][input]` for the commands in order and
/// then Done.
#[wasm_bindgen(js_name = phaseModel)]
pub fn phase_model() -> Object {
    let (states, commands, next) = crate::recipes::phase_model();
    let o = Object::new();
    set(
        &o,
        "states",
        states.into_iter().map(JsValue::from_str).collect::<Array>(),
    );
    set(
        &o,
        "commands",
        commands
            .into_iter()
            .map(JsValue::from_str)
            .collect::<Array>(),
    );
    let rows: Array = next
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|n| n.map_or(JsValue::NULL, JsValue::from_str))
                .collect::<Array>()
        })
        .map(JsValue::from)
        .collect();
    set(&o, "next", rows);
    o
}

#[cfg(feature = "simulator")]
pub use sim::JsSimulator;

#[cfg(feature = "simulator")]
mod sim {
    use super::{bytes, set};
    use super::{node_hex, parse_node};
    use crate::simulator::periph::SpiResponder;
    use crate::simulator::{PIN_COUNT, PinMode, SimDevice, SimEvent, Simulator};
    use js_sys::{Array, Function, Object};
    use middleware::wasm_abi::Pull;
    use wasm_bindgen::prelude::*;

    fn pull_name(p: Pull) -> &'static str {
        match p {
            Pull::None => "none",
            Pull::Up => "up",
            Pull::Down => "down",
        }
    }

    fn event_object(e: &SimEvent) -> Object {
        let o = Object::new();
        match e {
            SimEvent::Log(line) => {
                set(&o, "kind", "log");
                set(&o, "line", line.as_str());
            }
            SimEvent::Publish { key, data } => {
                set(&o, "kind", "publish");
                set(&o, "key", key.as_str());
                set(&o, "data", bytes(data));
            }
            SimEvent::Output(pin) => {
                set(&o, "kind", "output");
                set(&o, "pin", *pin);
            }
            SimEvent::Input(pin, pull) => {
                set(&o, "kind", "input");
                set(&o, "pin", *pin);
                set(&o, "pull", pull_name(*pull));
            }
            SimEvent::Write(pin, high) => {
                set(&o, "kind", "write");
                set(&o, "pin", *pin);
                set(&o, "high", *high);
            }
            SimEvent::Spi(t) => {
                set(&o, "kind", "spi");
                set(&o, "tx", bytes(&t.tx));
                set(&o, "rx", bytes(&t.rx));
            }
            SimEvent::Uart(data) => {
                set(&o, "kind", "uart");
                set(&o, "data", bytes(data));
            }
            SimEvent::PwmOpen { timer, freq_hz } => {
                set(&o, "kind", "pwmOpen");
                set(&o, "timer", *timer);
                set(&o, "freqHz", *freq_hz);
            }
            SimEvent::PwmSet {
                timer,
                channel,
                duty,
            } => {
                set(&o, "kind", "pwmSet");
                set(&o, "timer", *timer);
                set(&o, "channel", *channel);
                set(&o, "duty", *duty);
            }
            SimEvent::PwmStop(timer) => {
                set(&o, "kind", "pwmStop");
                set(&o, "timer", *timer);
            }
            SimEvent::Serve(key) => {
                set(&o, "kind", "serve");
                set(&o, "key", key.as_str());
            }
            SimEvent::Irq { event, handled } => {
                set(&o, "kind", "irq");
                set(&o, "irq", format!("{:?}", event.kind));
                set(&o, "code", event.kind as i32);
                set(&o, "source", event.source);
                set(&o, "value", event.value);
                set(&o, "timeUs", event.time_us);
                set(&o, "handled", *handled);
            }
        }
        o
    }

    /// JS numbers to a non-negative whole microsecond count.
    fn micros(now_us: f64) -> u64 {
        if now_us.is_finite() && now_us > 0.0 {
            now_us as u64
        } else {
            0
        }
    }

    /// A payload running in the device runtime (`wasm-host`) against a
    /// virtual device. Events — `{kind: "log", line}`, `{kind: "publish",
    /// key, data}`, `{kind: "output", pin}`, `{kind: "input", pin, pull}`,
    /// `{kind: "write", pin, high}`, `{kind: "spi", tx, rx}`, `{kind:
    /// "uart", data}` (UART 1 transmit), `{kind: "pwmOpen", timer,
    /// freqHz}`, `{kind: "pwmSet", timer, channel, duty}`, `{kind:
    /// "pwmStop", timer}` (ABI 3, successful calls only), `{kind: "serve",
    /// key}` (ABI 5) and `{kind: "irq", irq, code, source, value, timeUs,
    /// handled}` (an event popped for `on_irq`) — are passed to `onEvent`
    /// after each call. The device's node id is `node` (16 hex digits) or
    /// `0000000000000001`.
    #[wasm_bindgen(js_name = Simulator)]
    pub struct JsSimulator {
        sim: Simulator,
        on_event: Function,
    }

    #[wasm_bindgen(js_class = Simulator)]
    impl JsSimulator {
        /// Loads `wasm`, running its `init`, on a device with node id
        /// `node` (16 hex digits; the default when omitted). Events raised
        /// by `init` are delivered before this returns. Throws on
        /// rejection, trap or a malformed node id.
        #[wasm_bindgen(constructor)]
        pub fn new(
            module_bytes: &[u8],
            on_event: Function,
            node: Option<String>,
        ) -> Result<JsSimulator, JsError> {
            let mut device = SimDevice::new();
            if let Some(hex) = node {
                let id =
                    parse_node(&hex).ok_or_else(|| JsError::new("a node id is 16 hex digits"))?;
                device = device.with_node(id);
            }
            let sim = Simulator::load_on(module_bytes, None, device)
                .map_err(|e| JsError::new(&e.describe()))?;
            let mut s = JsSimulator { sim, on_event };
            s.flush();
            Ok(s)
        }

        fn flush(&mut self) {
            for e in self.sim.drain() {
                // A throwing JS callback must not unwind through the runtime.
                let _ = self.on_event.call1(&JsValue::NULL, &event_object(&e));
            }
        }

        fn finish<T>(&mut self, r: Result<T, crate::simulator::SimError>) -> Result<T, JsError> {
            self.flush();
            r.map_err(|e| JsError::new(&e.describe()))
        }

        /// `step(nowMs * 1000)`, discarding the count.
        pub fn tick(&mut self, now_ms: u32) -> Result<(), JsError> {
            let r = self.sim.tick(now_ms);
            self.finish(r)
        }

        /// One main-loop pass at `nowUs` microseconds since load: completes
        /// earlier DMA, raises due timers, runs `tick` if due and
        /// dispatches up to one queue of events to `on_irq`. Returns the
        /// number of events dispatched.
        pub fn step(&mut self, now_us: f64) -> Result<u32, JsError> {
            let r = self.sim.step(micros(now_us));
            self.finish(r).map(|n| n as u32)
        }

        /// Sets the clock in microseconds without running anything (input
        /// edges and UART bytes are stamped with it).
        #[wasm_bindgen(js_name = setNowUs)]
        pub fn set_now_us(&mut self, now_us: f64) {
            self.sim.set_time_us(micros(now_us));
        }

        /// Bytes arriving on UART 1; returns how many were buffered (0 when
        /// the payload has not opened it with an RX pin).
        #[wasm_bindgen(js_name = uartInject)]
        pub fn uart_inject(&mut self, data: &[u8]) -> u32 {
            self.sim.uart_inject(data) as u32
        }

        /// What the virtual SPI device answers: an empty pattern selects
        /// loopback, otherwise the pattern repeats.
        #[wasm_bindgen(js_name = setSpiPattern)]
        pub fn set_spi_pattern(&mut self, pattern: &[u8]) {
            self.sim
                .set_spi_responder(SpiResponder::from_pattern(pattern));
        }

        /// Sets the value `adc_read(pin)` returns (`0..=4095`; 0 until set).
        /// Throws for a larger value or a pin beyond `pinCount`.
        #[wasm_bindgen(js_name = setAdc)]
        pub fn set_adc(&mut self, pin: u8, value: u16) -> Result<(), JsError> {
            self.sim.set_adc(pin, value).map_err(|e| JsError::new(&e))
        }

        /// Open PWM timers: `[{timer, freqHz, duty: [d0, d1, d2, d3]}]`,
        /// duties as fractions of 65535.
        pub fn pwm(&mut self) -> Array {
            self.sim
                .pwm()
                .iter()
                .map(|t| {
                    let o = Object::new();
                    set(&o, "timer", t.timer);
                    set(&o, "freqHz", t.freq_hz);
                    let duty: Array = t.duty.iter().map(|&d| JsValue::from(d)).collect();
                    set(&o, "duty", duty);
                    o
                })
                .collect()
        }

        /// Started timers: `[{id, periodUs, periodic, active, fires}]`.
        pub fn timers(&mut self) -> Array {
            self.sim
                .timers()
                .iter()
                .map(|t| {
                    let o = Object::new();
                    set(&o, "id", t.id);
                    set(&o, "periodUs", t.period_us);
                    set(&o, "periodic", t.periodic);
                    set(&o, "active", t.active);
                    set(&o, "fires", t.fires);
                    o
                })
                .collect()
        }

        /// Events dropped from the full queue since load (not reset by the
        /// payload's `irq_overflow`).
        #[wasm_bindgen(getter)]
        pub fn dropped(&mut self) -> u32 {
            self.sim.dropped()
        }

        /// Events waiting for dispatch.
        #[wasm_bindgen(getter)]
        pub fn pending(&mut self) -> u32 {
            self.sim.pending() as u32
        }

        /// Whether the payload exports `on_irq`.
        #[wasm_bindgen(getter, js_name = handlesIrq)]
        pub fn handles_irq(&self) -> bool {
            self.sim.handles_irq()
        }

        /// Delivers a message; returns whether a subscription matched.
        pub fn deliver(&mut self, key: &str, data: &[u8]) -> Result<bool, JsError> {
            let r = self.sim.deliver(key, data);
            self.finish(r)
        }

        /// Hands a request to the payload serving `key` (ABI 5) with an
        /// optional deadline and sender (16 hex digits): `{ok: true,
        /// reply}` or `{ok: false, error}` with the `HandlerError` name a
        /// device would answer (`"NotFound"` when nothing serves the key).
        /// Throws on a trap (the payload stops) or a malformed key.
        pub fn request(
            &mut self,
            key: &str,
            body: &[u8],
            deadline_ms: Option<u32>,
            sender: Option<String>,
        ) -> Result<Object, JsError> {
            let sender = sender.as_deref().and_then(parse_node);
            let r = self.sim.request(key, body, deadline_ms, sender);
            let o = Object::new();
            match self.finish(r)? {
                Ok(reply) => {
                    set(&o, "ok", true);
                    set(&o, "reply", bytes(&reply));
                }
                Err(e) => {
                    set(&o, "ok", false);
                    set(&o, "error", crate::encode::status_name(Some(e)));
                }
            }
            Ok(o)
        }

        /// Keys the payload serves, in slot order.
        pub fn serves(&self) -> Array {
            self.sim.serves().into_iter().map(JsValue::from).collect()
        }

        /// Whether the payload exports `mw_alloc` and `on_request`.
        #[wasm_bindgen(getter, js_name = handlesRequests)]
        pub fn handles_requests(&self) -> bool {
            self.sim.handles_requests()
        }

        /// The device's node id (16 hex digits).
        #[wasm_bindgen(getter, js_name = nodeId)]
        pub fn node_id(&mut self) -> String {
            node_hex(self.sim.node())
        }

        /// Sets the time reported by `now_ms` without ticking.
        #[wasm_bindgen(js_name = setNow)]
        pub fn set_now(&mut self, now_ms: u32) {
            self.sim.set_now(now_ms);
        }

        /// Next tick deadline in ms, or `undefined`.
        #[wasm_bindgen(js_name = nextTick)]
        pub fn next_tick(&self) -> Option<u32> {
            self.sim.next_tick()
        }

        /// Subscribed keys.
        pub fn subscriptions(&self) -> Array {
            self.sim
                .subscriptions()
                .into_iter()
                .map(JsValue::from)
                .collect()
        }

        /// Drives a virtual input pin; returns whether it is an input.
        #[wasm_bindgen(js_name = setInput)]
        pub fn set_input(&mut self, pin: u8, high: bool) -> bool {
            self.sim.set_input(pin, high)
        }

        /// `{mode: "unused"|"output"|"input", high?, pull?}` for a pin, or
        /// `undefined` if out of range.
        pub fn pin(&mut self, pin: u8) -> Option<Object> {
            let o = Object::new();
            match self.sim.pin(pin)? {
                PinMode::Unused => set(&o, "mode", "unused"),
                PinMode::Output(high) => {
                    set(&o, "mode", "output");
                    set(&o, "high", high);
                }
                PinMode::Input(pull, level) => {
                    set(&o, "mode", "input");
                    set(&o, "pull", pull_name(pull));
                    set(&o, "high", level.unwrap_or(pull == Pull::Up));
                }
            }
            Some(o)
        }

        /// Number of virtual pins.
        #[wasm_bindgen(getter, js_name = pinCount)]
        pub fn pin_count(&self) -> u32 {
            PIN_COUNT as u32
        }

        /// Whether a trap stopped the payload.
        #[wasm_bindgen(getter)]
        pub fn trapped(&self) -> bool {
            self.sim.trapped()
        }
    }
}
