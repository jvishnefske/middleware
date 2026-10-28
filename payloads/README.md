# Payloads

Application payloads are small WebAssembly modules that the base firmware
runs in a fuel-metered sandbox. They talk to the device only through the ABI
defined in `src/wasm_abi.rs` of the `middleware` crate: host functions
imported from module `mw` (log, publish, subscribe, time, tick period, GPIO,
since ABI 2 GPIO interrupts, hardware timers, SPI and UART DMA, since
ABI 3 PWM, ADC and I2C, and since ABI 4 DMA routes that stream frames
between a key and a camera or SPI display, `mw_payload::route`) and the
exports `memory`, `init`, `tick`, `mw_alloc`, `on_message` and `on_irq`.

This directory is a Cargo workspace of its own, excluded from the parent
workspace, because its crates build for `wasm32-unknown-unknown` (set as the
default target in `.cargo/config.toml`). It contains:

- `sdk/` — the `mw-payload` crate, a `no_std` Rust SDK. Implement its
  `Payload` trait and invoke `mw_payload::payload!(YourType)` to get every
  export. Its crate documentation lists the API and explains the single,
  confined use of `unsafe` needed to call host imports. A payload crate can
  be `#![forbid(unsafe_code)]`, as every example here is: the macro adds no
  `allow`.
- `examples/blink` — toggles GPIO 8 every 500 ms and publishes the level on
  `app/blink/state`.
- `examples/echo` — republishes everything from `app/echo/in` on
  `app/echo/out` and logs each message.
- `examples/button` (ABI 2) — a falling-edge interrupt on GPIO 0 (the BOOT
  button of ESP32-S3 dev boards, pull-up enabled) toggles an LED on GPIO 8
  and publishes the debounced press count on `app/button/count`.
- `examples/spi_loopback` (ABI 2) — with GPIO 11 (MOSI) jumpered to GPIO 13
  (MISO), hardware timer 0 starts a 16-byte SPI DMA transfer every 100 ms;
  each completion compares the received bytes with those sent and
  publishes the running counts on `app/spi/ok` and `app/spi/err`. SCK is
  GPIO 12 and CS GPIO 10, the ESP32-S3 SPI2 defaults.
- `examples/uart_echo` (ABI 2) — echoes everything received on UART 1
  (GPIO 17 TX, GPIO 18 RX, 115200 baud) back with DMA writes and publishes
  each received line on `app/uart/line`; a line ends at a newline, a
  carriage return or 200 ms of silence.
- `examples/dimmer` (ABI 3) — every 100 ms reads ADC pin 0 (a
  potentiometer), sets PWM timer 0 channel 0 to the matching duty through
  `embedded-hal`'s `SetDutyCycle`, publishes the reading on
  `app/dimmer/adc`, and reads the temperature register of an LM75-style
  sensor at I2C address 0x48 through `embedded-hal`'s `I2c`, publishing it
  on `app/dimmer/temp`. The board decides which pins those are.
- `examples/cam_stream` (ABI 4, ESP32-CAM) — starts a source route that
  publishes 120×120 RGB565 frames on `app/cam/frame` every 200 ms as
  166-byte chunks, one ESP-NOW message each (174 per frame, 870 messages a
  second). The source is the synthetic test pattern until the firmware
  drives the camera behind `SourceKind::Camera`; switching is a one-constant
  change. Route events are counted and published every 2 s on
  `app/cam/stats` (frames, dropped frames, stops as three `u32` LE); a
  route that stops by itself is restarted.
- `examples/cam_view` (ABI 4, conagotchi badge) — brings up the badge's
  GC9A01 240×240 round panel on SPI2 (SCK 12, MOSI 11, CS 10, D/C 13,
  reset 14, 40 MHz): a hardware reset and the `mipidsi` GC9A01 init
  sequence with colour inversion and BGR order, each command and parameter
  run a DMA transfer sequenced through `DmaDone` and one-shot timer events
  by a pure, host-tested state machine, then a black fill. It then starts a
  sink route that draws `app/cam/frame` in the centre 120×120 window. A
  stopped route or failed step is retried after 1 s with a fresh bring-up.
  Built with 1-byte pages it fits the badge (16 KiB memory, 4 KiB stack,
  13 KiB image), and no call does more than one GPIO write and one transfer.
- `templates/` — the same programs written by hand in WAT and in
  AssemblyScript (blink, echo, button, a timer demo and the SPI loopback),
  used as starting points by the web editor. `templates/README.md` also
  explains the interrupt model in language-neutral terms.
- `abi-check/` — a host tool that validates any `.wasm` or `.wat` file against
  the ABI table in the `middleware` crate.
- `scripts/check-all.sh` — builds and validates everything in one go.

## Building

From this directory, `cargo build --release` produces one `.wasm` file per
example in `target/wasm32-unknown-unknown/release/`. The release profile
optimises for size, uses link-time optimisation, aborts on panic and strips
symbols; the examples come out at 5 to 13 KiB. An example that uses no ABI 2
(or ABI 3, or ABI 4) feature imports no function of that revision, so it still loads
on older firmware. The linker
flags in `.cargo/config.toml` give every payload exactly one 64 KiB page of
linear memory that cannot grow, with a 16 KiB stack, because the target
microcontrollers have only a few hundred KiB of RAM.

### 1-byte pages

Hosts that cannot spare a 64 KiB page (the ESP-NOW badge caps linear memory
at a few KiB) accept modules built with the WebAssembly custom-page-sizes
proposal, where a page is one byte and memory is sized to the byte.
`scripts/build-byte-pages.sh` builds with `-C link-arg=--page-size=1` and a
fixed linear memory of `MEMORY_BYTES` (default 16384) with a `STACK_BYTES`
(default 4096) stack, for example `scripts/build-byte-pages.sh -p blink -p
echo`. The output lands in `target/byte-pages/wasm32-unknown-unknown/release/`
so it never overwrites the 64 KiB-page build. Pointers and the ABI are the
same either way; only the host's byte cap (`wasm_host::Limits::bytes`) has
to be at least `MEMORY_BYTES`. `scripts/check-all.sh` builds and validates
blink, echo and cam_view both ways.

The byte-page build is reproducible: it passes `--remap-path-prefix` for
the home directory, the cargo registry and the repository, so no absolute
build path ends up in the module. `scripts/check-reproducible.sh [package...]`
builds here and in a copy of the sources at another path and fails unless
the two `.wasm` files are byte-identical; `check-all.sh` runs it for blink,
echo and cam_view.

The target must be installed once with
`rustup target add wasm32-unknown-unknown`.

## Validating

The validator runs on the host, so it is built with an explicit host target:
`cargo run -p abi-check --target x86_64-unknown-linux-gnu -- <files>` takes
any mix of `.wasm` and `.wat` files and prints, for each, the imports, the
exports and the memory limits, or every rule the module breaks. It checks
that every import comes from `mw` with exactly the name and signature listed
in the ABI table, that `memory` is exported, and that the optional exports
have the right signatures.

It also requires an `on_irq` export from any module that imports an event
source (`gpio_irq`, `timer_start`, `spi_transfer`, `uart_write`, and since
ABI 4 `route_source` and `route_sink`), since the events would otherwise
have nowhere to go.

`cargo test -p abi-check --target x86_64-unknown-linux-gnu` runs the same
checks on the WAT templates, on the built examples (build them first) and on
the compiled AssemblyScript templates when they exist. The SDK's pure logic
has host unit tests:
`cargo test -p mw-payload --target x86_64-unknown-linux-gnu`. The ABI 2
examples keep their decision logic (debouncing, the loopback verdict, line
splitting) in plain functions that are tested the same way, for example
`cargo test -p button --target x86_64-unknown-linux-gnu`.

`scripts/check-all.sh` does all of the above, runs clippy and rustdoc with
warnings denied, and compiles the AssemblyScript templates with
`npx assemblyscript@0.28` when Node is available.

## Simulating

The desktop simulator `payload-sim` lives in the parent workspace at
`tools/payload-sim` and runs a payload with the same runtime as the firmware.
From the repository root, `cargo run -p payload-sim -- <file.wasm> [--ticks N]`
instantiates the module, calls `init`, and then delivers `N` ticks, printing
logs, publications and GPIO changes.

For the ABI 2 examples the simulator provides virtual peripherals: hardware
timers run on its clock, a stdin line `!gpio <pin> <0|1>` changes an input
and raises the matching edge event, the SPI device loops MOSI back to MISO
(or answers a fixed pattern given with `--spi-pattern`), and `!uart <text>`
types at a UART 1 terminal whose transmit side is printed. With `--virtual`
the clock jumps from one due timer to the next, so `--until-ms 1000` runs a
second of `spi_loopback` instantly and deterministically.

For ABI 3, `!adc <pin> <0..4095>` sets the value an analog pin reads, PWM
duty changes are printed, and every I2C bus carries a 256-byte register
device at address 0x50 (write a register number followed by data to store
it; a read returns registers from the last register number written). Other
I2C addresses do not acknowledge.

For ABI 4 the simulator runs DMA routes on its clock: a test-pattern source
route prints one line per frame it publishes, and a sink route draws the
chunks arriving on its key on a virtual 240×240 GC9A01-like panel, printing
one line per frame drawn or dropped. `--peer <other.wasm>` runs a second
payload on its own board in the same process, with each board's
publications delivered to the other, so `cargo run -p payload-sim --
cam_view.wasm --peer cam_stream.wasm --virtual --until-ms 2000 --dump-panel
panel.ppm` runs the camera-to-display pipeline and writes the panel as a
PPM image at the end. `--loss <permille>` drops that share of the messages
between the two boards, reproducibly for a given `--seed <n>`.

## Uploading

Open the project's web page in Chrome, connect to a device running the base
firmware over Web Serial, choose the `.wasm` file in the payload panel and
upload it. The page sends it with the loader protocol, shows progress, and
starts the payload once the device has verified the CRC. The page also has an
editor that compiles the WAT and AssemblyScript templates in the browser and
a simulator that runs them without a device.
