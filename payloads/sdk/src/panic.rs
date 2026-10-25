//! Default `#[panic_handler]` (feature `panic-handler`, wasm32 only).
//!
//! Logs `panic at <file>:<line>` through the `log` import, without
//! `core::fmt`, then executes `unreachable` so the host sees a trap and stops
//! the payload. The panic message itself is not formatted, to keep the
//! handler small.

use crate::LogLine;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut line = LogLine::new();
    line.push_str("panic");
    if let Some(loc) = info.location() {
        line.push_str(" at ")
            .push_str(loc.file())
            .push_str(":")
            .push_u32(loc.line());
    }
    line.log();
    core::arch::wasm32::unreachable()
}
