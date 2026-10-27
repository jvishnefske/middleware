# Changelog

## 0.4.0

### Signed payloads (trust, breaking)
- New `loader::trust`: Ed25519 signed payload commits with a key pinned on first use. Wire types are always built (`PublicKey`, `Signature`, `Version`, `SignedCommit`, `TrustState`, `TrustInfo`, `TrustCode`, `Window`, `signed_message`); the new no_std feature `trust` adds SHA-256 hashing in the loader (`loader::sha256::ImageHash`, a `Copy` value over `sha2::compress256`) and `TrustState::claim` / `TrustState::accept_commit` (`ed25519-dalek` 2.2, `verify_strict`, no default features). (T1–T6)
- New system keys `sys/trust/info` and `sys/trust/claim`; `sys/loader/commit` takes an empty body or `[version u32][signature 64]`. (T1)
- **Breaking:** `Command::Commit` is `Command::Commit { auth }`; new `Command::TrustInfo` and `Command::TrustClaim { key }`. `Effect::Activate` is `Effect::Activate { image: ImageDigest, auth }`, where `ImageDigest` has `len`, `crc` and (with `trust`) `sha256`.
- **Breaking:** new `HandlerError::Denied`, wire code 5, gRPC `PERMISSION_DENIED` (and `UNAUTHENTICATED` maps back to it). Exhaustive matches on `HandlerError` need a new arm. (T10)
- `scripts/check-features.sh` also builds `trust` and `trust,serial` for `thumbv6m-none-eabi`. (T11)

### ESP base firmware (`firmware/`)
- `trust` enabled on every chip: the dispatcher keeps the pinned key, refuses unsigned, wrongly signed and non-increasing commits with `Denied` (the previous payload stays installed), answers `sys/trust/*`, and logs claims and refusals on `sys/log`. (T4–T6)
- New `trust` partition in `partitions.csv` (0x350000, 0x2000: two sectors written alternately, magic/format/sequence/CRC-32), `esp_base::trust_store`. A raised `min_version` is saved between the image CRC check and the slot header (`PayloadStore::activate_with`). (T8, T9)
- Re-key by physical presence: hold BOOT (GPIO0 on ESP32/ESP32-S3, GPIO9 on ESP32-C3) for 3 s after boot to open a 60 s window (`esp_base::presence`, shell task `presence::watch`). (T7)
- `Context` has a new `rekey_window` field; `Action::Activate` carries `trust`, and there is a new `Action::Claim` (with `trust`).
- The ESP8266 firmware builds `esp-base` without `trust`: `sys/trust/*` is `NotFound` and commits are not checked.

### mwctl and web
- `mwctl key new|pub <file>`, `mwctl trust info`, `mwctl trust claim --key <file>`, `mwctl upload <wasm> --key <file> [--version N]` (default version: Unix seconds); `Denied` explanations. (T12)
- protocol-wasm: `sha256`, `signedMessage`, `encodeSignedCommit`, `decodeTrustInfo`, `trustStateName`, `uploadPlanSigned`; the simulated `DeviceLoader` runs the trust rules (`setNow`, `assertPresence`, `trustInfo`); `statusName(5)` is "Denied". (T13)
- design.md: the host tool requirement T1 is renamed H1.

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
