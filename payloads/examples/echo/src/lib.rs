//! Echo: republishes everything received on [`IN_KEY`] on [`OUT_KEY`].
//!
//! `init` subscribes to [`IN_KEY`]; each `on_message` on that slot publishes
//! the same bytes on [`OUT_KEY`] and logs `echo: <payload>` (or
//! `echo: <N bytes>` for non-UTF-8 data).
#![no_std]
#![forbid(unsafe_code)]

use mw_payload::{LogLine, Payload, Slot};

/// Subscribed key.
pub const IN_KEY: &str = "app/echo/in";

/// Republished key.
pub const OUT_KEY: &str = "app/echo/out";

/// Application state: the subscription slot, if subscribing succeeded.
pub struct Echo {
    input: Option<Slot>,
}

impl Payload for Echo {
    fn init() -> Self {
        let input = match mw_payload::subscribe(IN_KEY) {
            Ok(slot) => Some(slot),
            Err(e) => {
                let mut line = LogLine::new();
                line.push_str("echo: subscribe error ")
                    .push_u32(e.code().unsigned_abs());
                line.log();
                None
            }
        };
        mw_payload::log("echo: started");
        Echo { input }
    }

    fn on_message(&mut self, slot: Slot, data: &[u8]) {
        if self.input != Some(slot) {
            return;
        }
        let mut line = LogLine::new();
        line.push_str("echo: ").push_bytes_lossy(data);
        if let Err(e) = mw_payload::publish(OUT_KEY, data) {
            line.push_str(" (publish error ")
                .push_u32(e.code().unsigned_abs())
                .push_str(")");
        }
        line.log();
    }
}

mw_payload::payload!(Echo);
