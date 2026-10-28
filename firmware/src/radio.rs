//! ESP-NOW policy: which keys travel over the air, on which channel, and
//! how the radio shell counts what it had to drop.
//!
//! The radio shell (`src/shell/radio.rs`, cargo feature `espnow`) runs the
//! middleware [`EspNowTransport`](middleware::espnow::EspNowTransport) over
//! esp-radio. Every decision it makes comes from here:
//!
//! * [`APP_FILTER`] (`Prefix("app")`) is the transport's only key filter, so
//!   the radio delivers `app` and `app/...` samples and nothing else;
//!   [`goes_over_air`] applies the same filter to the payload's publishes,
//!   so `app/...` keys are broadcast and every other key stays on the serial
//!   ports. System keys (`sys/...`) can therefore never leave the device by
//!   radio, nor be injected by one.
//! * [`CHANNEL`] is the fixed Wi-Fi channel. There is no access point to
//!   follow, so every device of a deployment must use the same channel;
//!   the conagotchi badge firmware parks on the same channel 1 when it is not
//!   associated.
//! * [`TX_DEPTH`] and [`RX_DEPTH`] bound the queues between the controller
//!   and the radio task. Neither side ever waits on them: a full queue drops
//!   the message and [`Counters`] counts it; [`Counters::report`] decides
//!   when the shell logs the counts on `sys/log`.
//! * [`SEND_TIMEOUT_MS`] bounds one broadcast (esp-radio completes it at the
//!   send callback, after the frame left the air), which is also what paces
//!   source-route chunks: at most one frame is queued in the Wi-Fi driver at
//!   any time.

use middleware::espnow::{HEADER_LEN, MAX_FRAME, MAX_KEY, MAX_PAYLOAD};
use middleware::{Filter, Key, KeyFilter};

/// Wi-Fi channel every device sends and listens on (2.412 GHz).
pub const CHANNEL: u8 = 1;

/// The root of the keys that travel over ESP-NOW.
pub const APP_ROOT: Key<'static> = Key::from_static("app");

/// The transport's key filter: `app` and every key below it.
pub const APP_FILTER: KeyFilter<'static> = Filter::Prefix(APP_ROOT);

/// Messages queued from the controller to the radio task (payload
/// publishes on `app/...` keys).
pub const TX_DEPTH: usize = 4;

/// Samples queued from the radio task to the controller.
pub const RX_DEPTH: usize = 8;

/// Longest a single broadcast may take, send callback included.
pub const SEND_TIMEOUT_MS: u64 = 20;

/// Longest key the radio carries ([`MAX_KEY`]).
pub const KEY_CAPACITY: usize = MAX_KEY;

/// Largest payload the radio sends: what fits one frame whatever the key.
pub const TX_PAYLOAD: usize = MAX_PAYLOAD;

/// Largest payload the radio can receive: a whole frame minus its header
/// and the shortest (one byte) key.
pub const RX_PAYLOAD: usize = MAX_FRAME - HEADER_LEN - 1;

/// Shortest interval between two `sys/log` reports of the counters.
pub const REPORT_INTERVAL_MS: u32 = 5_000;

/// Whether a publication on `key` is broadcast over ESP-NOW (it matches
/// [`APP_FILTER`] and fits the frame's key limit).
pub fn goes_over_air(key: Key<'_>) -> bool {
    APP_FILTER.matches(key) && key.as_bytes().len() <= MAX_KEY
}

/// Radio counters; every count saturates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Samples handed to the controller.
    pub rx: u32,
    /// Samples dropped because the controller's queue was full.
    pub rx_dropped: u32,
    /// Malformed `MW` frames and receive errors.
    pub rx_bad: u32,
    /// Frames broadcast (payload publishes and route chunks).
    pub tx: u32,
    /// Publishes dropped because the radio queue was full or the payload
    /// did not fit one frame.
    pub tx_dropped: u32,
    /// Broadcasts that failed or timed out.
    pub tx_failed: u32,
}

impl Counters {
    /// No counts.
    pub const fn new() -> Self {
        Counters { rx: 0, rx_dropped: 0, rx_bad: 0, tx: 0, tx_dropped: 0, tx_failed: 0 }
    }

    /// One sample delivered (`true`) or dropped on a full queue.
    pub const fn received(self, queued: bool) -> Self {
        if queued {
            Counters { rx: self.rx.saturating_add(1), ..self }
        } else {
            Counters { rx_dropped: self.rx_dropped.saturating_add(1), ..self }
        }
    }

    /// One malformed frame or receive error.
    pub const fn bad(self) -> Self {
        Counters { rx_bad: self.rx_bad.saturating_add(1), ..self }
    }

    /// One broadcast that succeeded (`true`) or failed.
    pub const fn sent(self, ok: bool) -> Self {
        if ok {
            Counters { tx: self.tx.saturating_add(1), ..self }
        } else {
            Counters { tx_failed: self.tx_failed.saturating_add(1), ..self }
        }
    }

    /// `n` publishes dropped before reaching the radio.
    pub const fn dropped(self, n: u32) -> Self {
        Counters { tx_dropped: self.tx_dropped.saturating_add(n), ..self }
    }

    /// Losses so far: everything dropped or failed.
    pub const fn losses(&self) -> u32 {
        self.rx_dropped
            .saturating_add(self.rx_bad)
            .saturating_add(self.tx_dropped)
            .saturating_add(self.tx_failed)
    }

    /// Whether to log the counters now: losses rose since the last report
    /// (`logged` losses then) and at least [`REPORT_INTERVAL_MS`] passed
    /// since it (`since_ms`).
    pub const fn report(&self, logged: u32, since_ms: u32) -> bool {
        self.losses() > logged && since_ms >= REPORT_INTERVAL_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r11_r12_only_app_keys_go_over_the_air() {
        let k = |s| Key::new(s).expect("valid");
        assert!(goes_over_air(k("app/cam/frame")));
        assert!(goes_over_air(k("app")));
        assert!(!goes_over_air(k("apple/x")), "segment-wise prefix");
        assert!(!goes_over_air(k("sys/log")));
        assert!(!goes_over_air(k("demo/x")));
        let long = "app/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(long.len() > MAX_KEY);
        assert!(!goes_over_air(k(long)), "longer than an ESP-NOW key");
        assert_eq!(CHANNEL, 1);
        assert_eq!(TX_PAYLOAD, 179);
        assert_eq!(RX_PAYLOAD, 242);
    }

    #[test]
    fn r11_r12_counters_saturate_and_report_on_new_losses() {
        let c = Counters::new().received(true).received(false).bad().sent(true).sent(false).dropped(2);
        assert_eq!(
            c,
            Counters { rx: 1, rx_dropped: 1, rx_bad: 1, tx: 1, tx_dropped: 2, tx_failed: 1 }
        );
        assert_eq!(c.losses(), 5);
        assert!(c.report(0, REPORT_INTERVAL_MS));
        assert!(!c.report(0, REPORT_INTERVAL_MS - 1), "rate limited");
        assert!(!c.report(5, u32::MAX), "nothing new");
        let full = Counters {
            rx: u32::MAX,
            rx_dropped: u32::MAX,
            rx_bad: u32::MAX,
            tx: u32::MAX,
            tx_dropped: u32::MAX,
            tx_failed: u32::MAX,
        };
        assert_eq!(full.received(true).received(false).bad().sent(true).sent(false).dropped(9), full);
        assert_eq!(full.losses(), u32::MAX);
    }
}
