# Changelog

## Unreleased

### ESP base firmware
- ESP32-S3: `heap_main` 240 → 176 KiB and the linear-memory cap 192 → 128 KiB (two pages), so the main stack grows from about 17 KiB to 81 KiB. Loading a payload (wasmi parse plus `init`, about 31 KiB deep) overflowed into the stack guard on hardware (2024 Kansas badge, ESP32-S3 v0.2); the echo payload now autostarts and echoes.

## 0.3.0

### Web: Blocks editor
- New "Blocks" tab (Blockly 13.3.0): events, publish/log, GPIO, timers, badge LEDs/text/pet/buttons and bounded built-ins generate one AssemblyScript module with only the "mw" imports it uses and fixed buffers. Compile → `checkAbi` → simulate → upload over Web Serial. Board profiles (esp-base, conagotchi badge) disable unsupported blocks; badge builds are rewritten to 1-byte pages (16 KiB). `web/check.sh` runs `npm test` and `npm run build`. (U9–U12)

### Core: owned keys and borrowing subscriptions (0.3, breaking)
- `key::KeyBuf<N>`: an owned, inline, validated key (`Copy`, no heap), with `AsKey` implemented for it and `Key`.
- `KeyFilter` is now `Filter<Key<'a>>`, an alias of the generic `Filter<K>`; `KeyFilterBuf<N> = Filter<KeyBuf<N>>` stores a filter. `Filter::{matches, as_filter, to_buf}` work on either. Constructing and matching `KeyFilter::Exact` / `KeyFilter::Prefix` is unchanged.
- `Subscribe::Subscriber<'a> where Self: 'a` and `subscribe<'a>(&'a mut self, ..)`: a subscriber may borrow its transport, so single-stream transports implement `Subscribe`. Implementations returning owned subscribers add the lifetime parameter and ignore it.
- `SampleMeta::seq` stays `u8`; its docs say why and how to compare it.

### Serial and ESP-NOW subscriptions (0.3)
- `SerialLink` implements `Subscribe`; its subscriber `Filtered<'l, IO>` (was `Filtered<'l, 'k, IO>`) borrows the link and owns a `KeyFilterBuf<MAX_KEY_LEN>`. **Breaking:** `SerialLink::filtered` is removed; use `link.subscribe(filter).await` (never waits, never fails).
- `EspNowTransport` implements `Subscribe`, handing out a new `EspNowSubscriber<'s, 'a, R, K, F, P>` that borrows the transport, owns a `KeyFilterBuf<MAX_KEY>` and selects with it instead of the filter slots. A filter key over `MAX_KEY` is `EspNowError::Frame(FrameError::KeyTooLong)`.
- `EspNowTransport<'a, R, K, F, P = MAX_FRAME>`: the new const generic `P` sizes the frame kept after `BufferTooSmall`. The default keeps behaviour unchanged; `P = 0` saves about 250 bytes of RAM, and a sample that does not fit the caller's buffer is then dropped instead of kept. `P > MAX_FRAME` is a compile error.

### gRPC client identity (0.3)
- `GrpcTransport` has a client id (`client_id()`, `with_client_id(SenderId)`, `grpc::client::fresh_client_id()`), sent as `mw-client` metadata (`bus::CLIENT_HEADER`) on publish and subscribe. Delivered samples carry the publisher in `SampleMeta.sender`.
- **Behaviour change:** like Zenoh, a transport no longer receives its own publications (clones share the id). `with_local_delivery(true)` opts back in (`mw-self: deliver`, `bus::SELF_HEADER`).
- Wire: subscribers that send `mw-client` get items `[key_len][key][sender_len][sender][payload]` (`bus::encode_item_with_sender` / `decode_item_with_sender`); subscribers without it keep the 0.2 format and delivery. `grpc::MAX_PAYLOAD` drops by 9 bytes to 4 MiB − 138 (`bus::ITEM_OVERHEAD`).

### BLE advertising (0.3)
- `AdvertisingSubscriber<S, K, L, D, F = K>`: `F` bounds the filters separately from the `K` learned keys. Filters are stored as owned `KeyFilterBuf<L>`; `subscribe_key`, `unsubscribe_key` and `with_filter` take any `Filter<Q: AsKey>` (a `KeyFilter` or a `KeyFilterBuf`), and `filters()` lists them. `ShortIdTable` stores `KeyBuf<L>`, so `L` may not exceed `MAX_KEY_LEN` (checked at compile time).
- New `ScanQueue<R = 8>`: a bounded, no-heap, waker-based `Scanner` that a radio callback fills with `push_report(&mut self, &ScanReport) -> Result<Pushed, ScanQueueError>` (newest wins when full). `next_report` waits for a push and is cancel-safe; `try_next_report` and `pop_report` do not wait; `poll_report` returns an owned `QueuedReport` for callers that share the queue behind their own lock.
- `AdvertisingSubscriber` implements `Subscribe`, handing out an `AdvertisingSubscription` view that delivers only its filter's keys; also `subscription()`, `scanner()`, `company_id()`.
- `const fn`: `AdvertisingSubscriber::with_company_id`, new `AdvertisingPublisher::with_template` (and `template()`), `ScanQueue::new`, `ShortIdTable::capacity`. `Default` for `AdvertisingPublisher` and `AdvertisingSubscriber` when the radio type is `Default`, and for `ScanQueue`.

### WASM runtime and payload SDK (0.3)
- **Breaking:** `wasm_host::LoadError::Init(Trap)` is now `LoadError::Init { trap: Trap, log: InitLog }`. `InitLog` holds the last `INIT_LOG_LINES` (8) lines `init` logged before it trapped (`lines()`, `len()`, `is_empty()`, `dropped()`, `Display`); the error's `Display` appends them. Lines still go to `Device::log` as before.
- **Breaking:** `Payload::tick` returns `Result<Ticked, Trap>` (was `Result<(), Trap>`), with `Ticked::{Ran, NotDue, Disabled, NoTickExport}`. Callers that only check for `Err` compile unchanged; new pure `abi::tick_decision`.
- `payload-sim` prints the log lines of a trapping `init` before `load failed: ...`.
- `mw_payload::payload!` no longer emits `#[allow(unsafe_code)]`, so payload crates may use `#![forbid(unsafe_code)]`; all examples now do.
- `payloads/scripts/build-byte-pages.sh` passes `--remap-path-prefix` for `$HOME`, the cargo registry and the repository; new `scripts/check-reproducible.sh` checks that two builds from different directories are byte-identical (run by `check-all.sh`).

### Payload ABI 3 (PWM, ADC, I2C)
- `wasm_abi::ABI_VERSION` is 3. New imports `pwm_open`, `pwm_set`, `pwm_stop`, `adc_read`, `i2c_open` and `i2c_transfer` (synchronous write, repeated start, read; at most `periph::MAX_I2C` = 64 bytes each way). ABI 1 and ABI 2 payloads load unchanged.
- `wasm_abi::periph` gains `PwmSpec`, `PwmDuty`, `I2cConfig`, `I2cTransfer` and the limits `MAX_PWM_TIMERS`, `PWM_CHANNELS`, `PWM_FREQ_HZ`, `ADC_MAX`, `I2C_BUSES`, `I2C_FREQ_HZ`, `MAX_I2C`.
- `wasm_host::Device` gains `pwm_open`, `pwm_set`, `pwm_stop`, `adc_read`, `i2c_open` and `i2c_transfer`, defaulting to `Unsupported`.
- The payload SDK adds `pwm` (`SetDutyCycle`), `adc` and `i2c` (`embedded_hal::i2c::I2c`); new `examples/dimmer`.
- `payload-sim` simulates PWM, ADC (`!adc <pin> <value>`) and an I2C register device at 0x50.

## 0.2.0

Breaking release driven by the conagotchi badge (ESP32-S3), which binds every backend to real radios and found where the 0.1 traits made it work around the crate.

### Core (`transport`, `key`, `dedup`)
- `KeyFilter { Exact, Prefix }` selects a family of keys, matching segment by segment; keys themselves still have no wildcards. A new `Subscribe` trait creates subscriptions at runtime.
- `Sample` gains `meta: SampleMeta { sender: Option<SenderId>, seq: Option<u8> }`. Build samples with `Sample::new(..).with_meta(..)`.
- `Subscriber::try_recv` (required) returns an already-available sample without waiting.
- The trait docs now state the cancel-safety rules: what happens when `recv`, `serve_one`, `request` or `publish` is dropped, the rule that sequences advance before the first await, and what to do when the receive buffer is too small.
- `Publisher::LIMITS: Limits { max_key, max_payload, max_message, shared }` with `Limits::new` (independent), `Limits::shared` (one frame budget) and `Limits::fits`.
- New `dedup` module:
  - `SeenTable<K>` deduplicates per (sender, key), so senders on one key no longer suppress each other.
  - Public `Sequence`, `fnv1a32`, `fnv1a64` and `short_id` (shared by BLE and ESP-NOW).
- `rust-version = "1.88"`.

### Backends
- **BLE advertising:**
  - per-sender dedup;
  - `ShortIdTable` resolves 16-bit ids to learned keys and detects collisions;
  - dynamic and prefix filters on `AdvertisingSubscriber`;
  - `Scanner::try_next_report`;
  - `Advertiser` may time-share the advertising set;
  - `AdvertisingPublisher::advertiser(&self)`.
- **BLE GATT:**
  - `GattLink` is split into `GattServer` and `GattClient`, which carry connection lifecycle events;
  - `BleServer` and `BleClient` replace `BleTransport`;
  - the MTU is read per message;
  - the ATT buffer size is a const generic;
  - `serve_one` is cancel-safe, and a disconnect or a new request id discards a partial request;
  - `with_services` is removed.
- **ESP-NOW (new `espnow` feature, no_std):** the MW v1 broadcast frame, runtime filters, per-sender dedup, sender and sequence metadata, and cancel-safe receive. It was upstreamed from conagotchi.
- **Zenoh:**
  - implements `Subscribe` (a prefix maps to `k/**`);
  - a sender and sequence attachment;
  - no self-delivery by default (`with_local_delivery` turns it back on).
- **gRPC bus (wire change):** `Subscribe` with an `mw-filter: exact|prefix` header, and keyed stream items `[key_len][key][payload]`. The inherent `subscribe(Key)` is removed.
- **Serial link:** `try_recv`, cancel-safe receive and transmit cursors, `filtered(KeyFilter)`, and `RECV_BUF_LEN`.
- **wasm-host:**
  - `Limits { max_memory_bytes, fuel_per_call }` with `Limits::pages` / `Limits::bytes`;
  - modules with 1-byte custom pages are accepted;
  - wasmi uses `portable-dispatch`, so stack use stays bounded on xtensa.
- **esp-base:** chip profiles state memory in bytes; the `sys/info` wire format is unchanged.
