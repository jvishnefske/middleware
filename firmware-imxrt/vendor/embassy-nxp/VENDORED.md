# Vendored embassy-nxp

A copy of `embassy-nxp` at embassy revision `d76de55c334d3221f9d05c6a8f7f0d3b7735c3fc`
(MIT OR Apache-2.0, see the upstream repository), used through `[patch]` in
`firmware-imxrt/Cargo.toml`. Changes from upstream:

1. `src/time_driver/pit.rs`: PERCLK divider ÷1 (24 MHz from the oscillator) instead of
   ÷24 (1 MHz). The PIT's register interface runs on PERCLK: on the MIMXRT1050-EVKB one
   register read took 297 core cycles at 1 MHz and 33 at 24 MHz (2026-11-14), and the time
   driver's ~15 accesses per timer event saturated the core once drivers polled every
   125-250 us (requests stopped being answered).
2. `Cargo.toml`: `time-driver-pit` selects `embassy-time/tick-hz-24_000_000` (was 1 MHz);
   the workspace-relative `path` dependencies point at the same git revision.
3. `src/time_driver/pit.rs` `on_interrupt`: the alarm is re-evaluated on every PIT
   interrupt instead of only when TFLG.TIF reads 1, and the TIF clear is read back before
   returning. Under frequent alarms the TFLG read returned a stale 0 while TIF was set, so
   the handler returned without clearing it and was re-entered forever (2026-11-14).

Note: `time-driver-pit`'s `embassy-time?/tick-hz-*` only takes effect when embassy-nxp's
own optional embassy-time dependency is enabled; imxrt-base selects
`embassy-time/tick-hz-24_000_000` itself (without it, embassy-time assumed 1 MHz and time
ran 24 times fast).
