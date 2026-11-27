//! What the fixed containers cost. A payload's host state (keys, the DMA
//! copy buffer, the channel table, `init`'s log) is inline wherever the
//! payload is kept, so its size is the price of not allocating it.

use core::mem::size_of;
use wasm_host::link::State;
use wasm_host::{InitLog, LoadError};

#[test]
fn the_inline_state_stays_within_its_budget() {
    let (state, log, error) = (
        size_of::<State<()>>(),
        size_of::<InitLog>(),
        size_of::<LoadError>(),
    );
    println!("State<()> {state} B, InitLog {log} B, LoadError {error} B");
    // 64-bit host figures; a 32-bit target is smaller (lengths are words).
    assert!(state <= 16 * 1024, "{state}");
    assert!(log <= 2200, "{log}");
    // The log is not inline in the error: only a failed `init` boxes it.
    assert!(error <= 64, "{error}");
}
