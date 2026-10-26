# Changelog

## Unreleased

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
