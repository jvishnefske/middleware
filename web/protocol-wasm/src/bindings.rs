//! `wasm-bindgen` exports: the JS-facing surface of the crate.
//!
//! Everything here converts between JS values and the pure modules
//! ([`crate::decoder`], [`crate::encode`], [`crate::loader`],
//! [`crate::pins`], [`crate::machines`], [`crate::simulator`]); no
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
                event: text(&a, "event"),
            }),
            other => Err(JsError::new(&format!(
                "{at}[{k}].kind {other:?} is not an action"
            ))),
        })
        .collect()
}

/// Checks state machines against the pin table (see `crate::machines` and
/// the `state-table` crate). `machines` is `[{id, name, initial, states:
/// [{name, note, entry: ActionRef[]}], events: [{name, note}], rows:
/// [{state, event, guard: Expr|null, next, actions: ActionRef[]}]}]` with
/// `Expr` `{kind: "pin", label} | {kind: "const", value} | {kind: "cmp",
/// op, a, b} | {kind: "and"|"or", a, b} | {kind: "not", a}` and `ActionRef`
/// `{kind: "write", label, level: 0|1} | {kind: "toggle", label} | {kind:
/// "publish", key, text} | {kind: "log", text} | {kind: "raise", event}`
/// (`web/src/model/machines.ts`). `pins` is `[{label, func}]` with `func`
/// a pin-table function name (`"gpio-out"`, `"gpio-in"`, ...). Expressions
/// nested deeper than the limit are reported as issues, not read; an
/// unknown expression or action kind, or a non-integer constant, throws.
/// Returns `[{machine, list: "machine"|"states"|"events"|"rows", row:
/// number|null, field, severity: "error"|"warning", message, fix?:
/// {kind: "add-state"|"add-event"|"set-initial", name} | {kind: "add-pin",
/// label, func}}]`.
#[wasm_bindgen(js_name = checkMachines)]
pub fn check_machines(machines: Array, pins: Array) -> Result<Array, JsError> {
    use crate::machines::{MachineFix, MachineIn, PinIn, RowIn, StateIn, check, flatten};
    use crate::pins::Func;
    let pins: Vec<PinIn> = pins
        .iter()
        .map(|p| {
            let func = text(&p, "func");
            PinIn {
                label: text(&p, "label"),
                func: Func::parse(&func).ok_or(func),
            }
        })
        .collect();
    let machines: Vec<MachineIn> = machines
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
        .collect::<Result<_, JsError>>()?;
    Ok(check(&machines, &pins)
        .into_iter()
        .map(|i| {
            let o = Object::new();
            set(&o, "machine", i.machine as u32);
            set(&o, "list", i.list);
            set(
                &o,
                "row",
                i.row.map_or(JsValue::NULL, |r| JsValue::from(r as u32)),
            );
            set(&o, "field", i.field);
            set(&o, "severity", i.severity);
            set(&o, "message", i.message.as_str());
            if let Some(fix) = i.fix {
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
                set(&o, "fix", f);
            }
            JsValue::from(o)
        })
        .collect())
}

#[cfg(feature = "simulator")]
pub use sim::JsSimulator;

#[cfg(feature = "simulator")]
mod sim {
    use super::{bytes, set};
    use crate::simulator::periph::SpiResponder;
    use crate::simulator::{PIN_COUNT, PinMode, SimEvent, Simulator};
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
    /// "pwmStop", timer}` (ABI 3, successful calls only) and `{kind: "irq",
    /// irq, code, source, value, timeUs, handled}` (an event popped for
    /// `on_irq`) — are passed to `onEvent` after each call.
    #[wasm_bindgen(js_name = Simulator)]
    pub struct JsSimulator {
        sim: Simulator,
        on_event: Function,
    }

    #[wasm_bindgen(js_class = Simulator)]
    impl JsSimulator {
        /// Loads `wasm`, running its `init`. Events raised by `init` are
        /// delivered before this returns. Throws on rejection or trap.
        #[wasm_bindgen(constructor)]
        pub fn new(module_bytes: &[u8], on_event: Function) -> Result<JsSimulator, JsError> {
            let sim =
                Simulator::load(module_bytes, None).map_err(|e| JsError::new(&e.describe()))?;
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
