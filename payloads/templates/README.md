# Payload templates

Every file here is a complete payload that the web editor offers as a
starting point. The `.wat` files are hand-written WebAssembly text; the
`.ts` files are AssemblyScript.

- `blink.wat` and `blink.ts` toggle GPIO 8 every 500 ms and publish `1` or
  `0` on `app/blink/state`, like the Rust `blink` example.
- `echo.wat` and `echo.ts` republish everything from `app/echo/in` on
  `app/echo/out`, like the Rust `echo` example.
- `button.wat` and `button.ts` (ABI 2) take a falling-edge interrupt on
  GPIO 0, the BOOT button of ESP32-S3 dev boards, with the pull-up enabled.
  Each press, debounced by 20 ms using the interrupt timestamp, toggles the
  LED on GPIO 8 and publishes the press count in decimal on
  `app/button/count`, like the Rust `button` example.
- `timer.wat` (ABI 2) starts hardware timer 0 with a 500 ms period and, on
  each alarm, toggles GPIO 8 and publishes the total number of periods on
  `app/timer/count`.
- `spi.ts` (ABI 2) is the SPI loopback test of the Rust `spi_loopback`
  example: with GPIO 11 (MOSI) jumpered to GPIO 13 (MISO), timer 0 starts a
  16-byte DMA transfer every 100 ms, and the completion handler reads the
  received bytes back, compares them with what was sent and publishes the
  running counts on `app/spi/ok` and `app/spi/err`.

## The ABI in AssemblyScript terms

The authoritative definition is `src/wasm_abi.rs` in the `middleware` crate,
with `src/wasm_abi/events.rs` and `src/wasm_abi/periph.rs` for the ABI 2
additions. Everything below restates it for AssemblyScript and WAT authors.
ABI 2 is a superset of ABI 1: a payload that uses none of the ABI 2 imports
and does not export `on_irq` loads on either firmware.

### Host functions

Every host function is imported from module `mw`. In AssemblyScript that
means declaring it with the `@external("mw", "<name>")` decorator on a
`declare function`. All parameters and results are 32-bit integers: use `i32`
for numbers and `usize` for pointers, which is also 32 bits on wasm32.

- `log(ptr, len)` writes one UTF-8 log line, truncated at 256 bytes.
- `publish(keyPtr, keyLen, dataPtr, dataLen)` publishes up to 512 bytes on a
  key and returns 0 or an error.
- `subscribe(keyPtr, keyLen)` returns a subscription slot (0 to 7) or an
  error; at most eight subscriptions may be active.
- `now_ms()` returns milliseconds since the payload started, wrapping.
- `set_tick(periodMs)` sets how often `tick` is called; 0 disables it, and
  periods under 10 ms are rejected.
- `gpio_output(pin)` and `gpio_input(pin, pull)` configure a pin, where pull
  is 0 for none, 1 for pull-up and 2 for pull-down. Pins the firmware needs
  for flash, USB or its console are refused.
- `gpio_write(pin, level)` drives an output to 0 or 1; `gpio_read(pin)`
  returns 0 or 1.

ABI 2 adds interrupts, hardware timers and DMA:

- `now_us()` returns microseconds since boot, wrapping about every 71
  minutes; compare times by subtracting them as unsigned 32-bit values.
- `irq_overflow()` returns how many events were dropped because the event
  queue was full since the previous call, and resets the count.
- `gpio_irq(pin, edge)` enables an interrupt on a pin already configured as
  an input. The edge is 0 for off, 1 for rising, 2 for falling and 3 for
  either. Turning it off also discards the pin's queued events.
- `timer_start(id, periodUs, periodic)` starts hardware timer 0 to 3 with a
  period of at least 100 µs; periodic is 1 to re-arm after each alarm or 0
  for a single alarm. `timer_stop(id)` stops it and discards its queued
  events.
- `spi_open(bus, sck, mosi, miso, cs, freqKhz, mode)` configures SPI bus 0
  as a master. A pin argument of -1 means unused, but the clock pin and at
  least one of MOSI and MISO are required and pins must be distinct. The
  clock is 1 to 40000 kHz and mode is 0 to 3.
- `spi_transfer(bus, txPtr, txLen, rxLen)` starts a transfer of the larger
  of the two lengths (1 to 4092 bytes; transmit is padded with zeros) and
  returns 0 at once.
- `dma_read(channel, ptr, len)` copies up to `len` bytes received by the
  last finished SPI transfer into memory and returns the count. Channel 0 is
  SPI bus 0.
- `uart_open(port, tx, rx, baud)` opens the secondary UART; the port must
  be 1, because UART 0 carries the device protocol. At least one pin is
  required and the baud rate is 300 to 3000000.
- `uart_write(port, ptr, len)` starts transmitting 1 to 4092 bytes and
  returns 0 at once; `uart_read(port, ptr, len)` copies received bytes that
  the firmware has buffered (up to 1024) and returns the count, which may
  be 0.

A negative result is an error code: -1 invalid argument, -2 a pointer and
length outside memory, -3 no resources left, -4 unsupported on this device,
-5 the message could not be sent, -6 a malformed or reserved key, and -7
busy, meaning a DMA transfer on that peripheral is still in flight or the
peripheral was not opened. Keys under `sys/` belong to the firmware and are
always refused.

Strings must be passed as UTF-8 bytes, but AssemblyScript strings are UTF-16.
The templates therefore encode each constant string once, at start-up, with
`String.UTF8.encode`, and pass `changetype<usize>(buffer)` and
`buffer.byteLength` to the host. Encoding inside `tick` or `on_message` would
allocate on every call, and with the stub runtime that memory is never
returned.

### Exports

A module compiled by `asc` exports its `memory` automatically. Everything else
is an ordinary `export function`:

- `init(): void` runs once after the module is loaded. Subscribe, configure
  pins and call `set_tick` here.
- `tick(nowMs: i32): void` runs every tick period.
- `mw_alloc(len: i32): usize` and `on_message(slot: i32, ptr: usize, len:
  i32): void` are required as soon as the payload subscribes. To deliver a
  message the host first asks `mw_alloc` for a buffer of `len` bytes, copies
  the message into it, and then calls `on_message` with that pointer. A
  static buffer from `memory.data(512)` is all a payload needs; returning 0
  from `mw_alloc` declines the message.

- `on_irq(kind: i32, source: i32, value: i32, timeUs: i32): void` (ABI 2)
  receives interrupt and DMA events, described in the next section. It is
  required as soon as the payload imports `gpio_irq`, `timer_start`,
  `spi_transfer` or `uart_write`, because their results arrive only through
  it; `abi-check` rejects a module that imports one of them without
  exporting `on_irq`.

Each call into the payload runs on a fixed fuel budget, so every export must
return promptly; an endless loop traps instead of hanging the device.

### The interrupt model

Payload code never runs in interrupt context. When a pin changes, a timer
fires, a DMA transfer completes or the secondary UART receives bytes, the
firmware's interrupt handler only records the event with a microsecond
timestamp and appends it to a bounded queue of 32 entries. The firmware's
main loop later takes events from the queue in order and calls `on_irq` once
for each. Calls into a payload never overlap: `on_irq`, `tick` and
`on_message` each run to completion before the next starts, so a payload
needs no locking, and every handler may freely publish, log and start new
transfers. The price is latency: the handler runs when the main loop gets
to it, so timing measurements should use the `timeUs` argument, taken in the
interrupt handler, rather than calling `now_us`.

The `kind` argument says what happened and how to read the other three:

- 1, GPIO edge: `source` is the pin and `value` the level after the edge.
- 2, timer alarm: `source` is the timer id and `value` the number of
  periods since the payload last saw this timer, at least 1.
- 3, DMA done: `source` is the channel (0 for SPI bus 0, 1 for UART 1
  transmit) and `value` the number of bytes transferred, or a negative
  error code.
- 4, UART receive: `source` is the port and `value` the number of bytes
  now buffered.

A payload should ignore kinds it does not know, because a later ABI may add
more.

Two kinds coalesce so that a fast source cannot fill the queue. A timer
alarm that fires while the same timer's previous event is still queued is
merged into it, which is why `value` counts periods; a payload that adds up
`value` never loses a period. A UART receive event likewise merges into a
queued one and carries the latest byte count. GPIO edges and DMA
completions are never merged. When the queue is full, a new event that
cannot be merged is dropped and counted, and `irq_overflow` reports the
count. A bouncing button can fill the queue on its own, so debounce using
the timestamps and check the overflow count. A dropped DMA completion means
the payload never learns that a transfer finished; the templates therefore
give up waiting after a number of timer periods, and the firmware's own
busy check decides whether the next transfer may start.

DMA never touches the payload's memory. `spi_transfer` and `uart_write`
copy the transmit bytes into buffers owned by the firmware before
returning, so the payload may overwrite its buffer straight away, and
`dma_read` copies received bytes back only when the payload asks. This
keeps a transfer safe even if the interpreter moves the payload's memory,
and it means a second transfer on the same peripheral is refused with -7
until the DMA done event for the first has arrived.

Code in the top level of an AssemblyScript file runs while the module is
being instantiated, before `init`. Keep host calls out of it.

### Compiler options

The templates are compiled with `--runtime stub`, `--use abort=`, `-O` and
`--initialMemory 1 --maximumMemory 1`. The stub runtime is a bump allocator
with no garbage collector, which keeps the module around 1 KiB. Without
`--use abort=` the compiler imports `env.abort`, which the device does not
provide, and the upload would be rejected; with it, a failed assertion
simply traps. The memory options pin the module to one 64 KiB page, matching
what the firmware provides.

## Hand-written WAT

The WAT templates follow the same contract with explicit memory offsets:
constant strings sit in data segments near the start of memory, echo keeps
its 512-byte receive buffer at offset 1024, and button and timer format
their decimal counts in a small scratch area at offset 96. They declare
exactly one memory page with no room to grow.

## Trying the templates

`abi-check` validates the `.wat` files directly. The desktop simulator
`payload-sim` loads binary modules only, so compile a WAT template first,
for example with `npx wabt wat2wasm`, or use the AssemblyScript output that
`scripts/check-all.sh` leaves in `target/as`. In the simulator, a stdin line
`!gpio 0 0` presses the button, `!gpio 0 1` releases it, timers run on the
simulator clock, and the virtual SPI device loops MOSI back to MISO.
