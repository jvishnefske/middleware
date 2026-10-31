//! ESP-NOW policy: which keys travel over the air, on which channel, and
//! how the radio shell counts what it had to drop.
//!
//! The radio shell (`src/shell/radio.rs`, cargo feature `espnow`) runs the
//! middleware [`EspNowTransport`](middleware::espnow::EspNowTransport) over
//! esp-radio. Every decision it makes comes from here:
//!
//! * [`FILTERS`] are the transport's key filters: [`APP_FILTER`]
//!   (`Prefix("app")`) and [`DISC_FILTER`] (`Prefix("disc")`, the
//!   discovery beacons, [`disc`]), so the radio delivers `app/...` and
//!   `disc/...` samples and nothing else. A payload may only send on
//!   `app/...`: [`goes_over_air`] (its publishes) and [`carries_routes`]
//!   (its source routes) admit `app/...` alone, so every other key stays on
//!   the serial ports. `disc/...` is reserved for the radio task's own
//!   beacon ([`disc::cam_beacon`]), so a payload cannot forge discovery
//!   beacons, and system keys (`sys/...`) are never published over the
//!   radio, nor delivered from it as publications.
//! * Requests arrive separately, as `middleware::rpc` frames (`MR`, not the
//!   publications' `MW`) addressed to this station: the shell taps them off
//!   the radio before the transport (which skips them as foreign), answers
//!   repeats from a [`Replies`] cache of [`RPC_CACHE`] responses and hands
//!   new ones to the controller, which admits only the over-the-air update
//!   keys on a claimed device, with signed commits and signed start/stop
//!   ([`crate::ota`]). Responses go back unicast to the requester; [`Peers`]
//!   decides which of at most [`RPC_PEERS`] requesters stay registered with
//!   the Wi-Fi driver. Only the radio task builds `MR` frames: a payload's
//!   publishes and routes are `MW` samples on `app/...` and cannot reach
//!   the request path.
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

pub mod disc;

use middleware::discovery::beacon::BEACON_PREFIX;
use middleware::espnow::{HEADER_LEN, MAX_FRAME, MAX_KEY, MAX_PAYLOAD};
use middleware::rpc::Replies;
use middleware::{Filter, Key, KeyFilter};

/// A 48-bit ESP-NOW (Wi-Fi MAC) address.
pub type Mac = [u8; 6];

/// Wi-Fi channel every device sends and listens on (2.412 GHz).
pub const CHANNEL: u8 = 1;

/// The root of the keys that travel over ESP-NOW.
pub const APP_ROOT: Key<'static> = Key::from_static("app");

/// The application keys: `app` and every key below it.
pub const APP_FILTER: KeyFilter<'static> = Filter::Prefix(APP_ROOT);

/// The discovery beacons: `disc/<kind>` (`middleware::discovery::beacon`).
pub const DISC_FILTER: KeyFilter<'static> = Filter::Prefix(BEACON_PREFIX);

/// The transport's key filters, in slot order: every key admitted on the
/// radio, in either direction.
pub const FILTERS: [KeyFilter<'static>; 2] = [APP_FILTER, DISC_FILTER];

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

/// Request frames queued from the radio receive path to the radio task.
pub const RPC_DEPTH: usize = 4;

/// Responses remembered for answering repeats.
pub const RPC_CACHE: usize = 8;

/// Largest cached response body: the controller's largest reply
/// (`sys/info`, at most `MAX_INFO_LEN` = 81 bytes with the node id, fits).
pub const RPC_BODY: usize = 128;

/// Requesters kept registered as unicast peers with the Wi-Fi driver.
pub const RPC_PEERS: usize = 4;

/// Longest a unicast response may take, send callback (and the peer's MAC
/// acknowledgement or the driver's retries) included.
pub const RPC_SEND_TIMEOUT_MS: u64 = 50;

/// The radio task's responder state.
pub type RpcReplies = Replies<RPC_CACHE, RPC_BODY>;

/// What to do with the Wi-Fi driver's peer list before a unicast send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerChange {
    /// The peer is registered already.
    Known,
    /// Register it.
    Add,
    /// Unregister the least recently used peer, then register this one.
    Replace(Mac),
}

/// The requesters registered as unicast peers, least recently used first
/// out. A value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peers<const N: usize> {
    macs: [Option<(Mac, u32)>; N],
    clock: u32,
}

impl<const N: usize> Default for Peers<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Peers<N> {
    /// No peers.
    pub const fn new() -> Self {
        Peers { macs: [None; N], clock: 0 }
    }

    /// Records a send to `mac`; returns the next table and the driver
    /// change it needs. With `N == 0` every peer is added and never
    /// remembered.
    pub fn send_to(self, mac: Mac) -> (Self, PeerChange) {
        let mut next = self;
        next.clock = next.clock.wrapping_add(1);
        let clock = next.clock;
        if let Some(slot) = next.macs.iter_mut().flatten().find(|(m, _)| *m == mac) {
            slot.1 = clock;
            return (next, PeerChange::Known);
        }
        if let Some(slot) = next.macs.iter_mut().find(|s| s.is_none()) {
            *slot = Some((mac, clock));
            return (next, PeerChange::Add);
        }
        let oldest = (0..N).max_by_key(|&i| next.macs.get(i).copied().flatten().map_or(0, |(_, t)| clock.wrapping_sub(t)));
        match oldest.and_then(|i| next.macs.get_mut(i)) {
            Some(slot) => {
                let old = slot.map(|(m, _)| m);
                *slot = Some((mac, clock));
                (next, old.map_or(PeerChange::Add, PeerChange::Replace))
            }
            None => (next, PeerChange::Add),
        }
    }
}

/// Shortest interval between two `sys/log` reports of the counters.
pub const REPORT_INTERVAL_MS: u32 = 5_000;

/// Whether a payload's publication on `key` is broadcast over ESP-NOW: it
/// matches [`APP_FILTER`] and fits the frame's key limit. `disc/...` is
/// refused (reserved for the radio task's own beacon), as is every other
/// key.
pub fn goes_over_air(key: Key<'_>) -> bool {
    APP_FILTER.matches(key) && key.as_bytes().len() <= MAX_KEY
}

/// Whether a payload's source route may stream on `key`: the same
/// `app/...` rule as [`goes_over_air`].
pub fn carries_routes(key: Key<'_>) -> bool {
    goes_over_air(key)
}

/// Whether a received sample on `key` is a discovery beacon.
pub fn is_beacon(key: Key<'_>) -> bool {
    DISC_FILTER.matches(key)
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

/// Request/response counters of the radio task; every count saturates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RpcCounters {
    /// Requests handed to the controller.
    pub executed: u32,
    /// Repeats answered from the cache.
    pub replayed: u32,
    /// Requests dropped while another was in flight (the requester resends).
    pub busy: u32,
    /// Requests dropped before the controller: the tap queue or the
    /// controller's inbound queue was full, or the frame was malformed.
    pub dropped: u32,
    /// Responses whose unicast send failed or timed out.
    pub tx_failed: u32,
}

impl RpcCounters {
    /// No counts.
    pub const fn new() -> Self {
        RpcCounters { executed: 0, replayed: 0, busy: 0, dropped: 0, tx_failed: 0 }
    }

    /// One request handed to the controller.
    pub const fn executed(self) -> Self {
        RpcCounters { executed: self.executed.saturating_add(1), ..self }
    }

    /// One repeat answered from the cache.
    pub const fn replayed(self) -> Self {
        RpcCounters { replayed: self.replayed.saturating_add(1), ..self }
    }

    /// One request dropped while another was in flight.
    pub const fn busy(self) -> Self {
        RpcCounters { busy: self.busy.saturating_add(1), ..self }
    }

    /// `n` requests dropped before the controller.
    pub const fn dropped(self, n: u32) -> Self {
        RpcCounters { dropped: self.dropped.saturating_add(n), ..self }
    }

    /// One response that could not be sent.
    pub const fn tx_failed(self) -> Self {
        RpcCounters { tx_failed: self.tx_failed.saturating_add(1), ..self }
    }

    /// Whether to log the counters now: they changed since the last report
    /// (`logged`) and at least [`REPORT_INTERVAL_MS`] passed since it.
    pub fn report(&self, logged: &RpcCounters, since_ms: u32) -> bool {
        self != logged && since_ms >= REPORT_INTERVAL_MS
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
        assert!(carries_routes(k("app/cam/frame")) && !carries_routes(k("disc/cam/jpeg")));
        let long = "app/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(long.len() > MAX_KEY);
        assert!(!goes_over_air(k(long)), "longer than an ESP-NOW key");
        assert_eq!(CHANNEL, 1);
        assert_eq!(TX_PAYLOAD, 179);
        assert_eq!(RX_PAYLOAD, 242);
    }

    #[test]
    fn o8_unicast_peers_are_bounded_and_rpc_counters_saturate() {
        let m = |b: u8| [2, 0, 0, 0, 0, b];
        let p = Peers::<2>::new();
        let (p, c) = p.send_to(m(1));
        assert_eq!(c, PeerChange::Add);
        let (p, c) = p.send_to(m(2));
        assert_eq!(c, PeerChange::Add);
        let (p, c) = p.send_to(m(1));
        assert_eq!(c, PeerChange::Known);
        let (p, c) = p.send_to(m(3));
        assert_eq!(c, PeerChange::Replace(m(2)), "2 is the least recently used");
        let (p, c) = p.send_to(m(1));
        assert_eq!(c, PeerChange::Known);
        let (_, c) = p.send_to(m(2));
        assert_eq!(c, PeerChange::Replace(m(3)));
        assert_eq!(Peers::<0>::new().send_to(m(1)).1, PeerChange::Add);
        let c = RpcCounters::new().executed().replayed().busy().dropped(2).tx_failed();
        assert_eq!(c, RpcCounters { executed: 1, replayed: 1, busy: 1, dropped: 2, tx_failed: 1 });
        assert!(c.report(&RpcCounters::new(), REPORT_INTERVAL_MS));
        assert!(!c.report(&RpcCounters::new(), REPORT_INTERVAL_MS - 1), "rate limited");
        assert!(!c.report(&c, u32::MAX), "nothing new");
        let full = RpcCounters { executed: u32::MAX, replayed: u32::MAX, busy: u32::MAX, dropped: u32::MAX, tx_failed: u32::MAX };
        assert_eq!(full.executed().replayed().busy().dropped(3).tx_failed(), full);
        const { assert!(RPC_BODY >= middleware::loader::MAX_INFO_LEN, "sys/info (with the 0.5 node id) fits the cache") };
    }

    /// The combined air policy (E18 with O6/O16): a payload's publishes and
    /// source routes are `MW` samples on `app/...` only, so a payload can
    /// send neither a `disc/...` beacon nor any `sys/...` key, and nothing it
    /// sends is a `MR` request frame, the only way `sys/...` requests (the
    /// OTA keys, admitted by `crate::ota`) cross the radio.
    #[test]
    fn e18_o6_a_payload_reaches_neither_discovery_nor_the_request_path() {
        use middleware::espnow::Frame;
        use middleware::loader::keys;
        use middleware::rpc::{Message, is_rpc};
        let ota = [
            keys::INFO,
            keys::TRUST_INFO,
            keys::TRUST_CLAIM,
            keys::BEGIN,
            keys::CHUNK,
            keys::COMMIT,
            keys::START,
            keys::STOP,
        ];
        for key in ota.iter().chain(&[Key::new("disc/cam/jpeg").expect("valid")]) {
            assert!(!goes_over_air(*key) && !carries_routes(*key), "{key:?}");
        }
        for key in ota {
            assert!(!FILTERS.iter().any(|f| f.matches(key)), "{key:?} is never received as a sample");
        }
        // An `app/...` publish carrying a whole request frame (a would-be
        // `sys/payload/stop` with a 72-byte body) is still a sample: its
        // frame starts with `MW`, so the request tap never sees it.
        let mut req = [0u8; MAX_FRAME];
        let msg = Message::Request { id: 1, to: [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24], key: keys::STOP, body: &[0; 72] };
        let n = msg.encode(&mut req).expect("fits");
        let inner = req.get(..n).expect("encoded");
        assert!(is_rpc(inner));
        let key = Key::new("app/x").expect("valid");
        assert!(goes_over_air(key));
        let mut frame = [0u8; MAX_FRAME];
        let m = Frame { seq: 0, key, payload: inner }.encode(&mut frame).expect("fits");
        let sent = frame.get(..m).expect("encoded");
        assert!(!is_rpc(sent) && Message::decode(sent).is_err());
        assert_eq!(Frame::decode(sent).map(|f| f.payload), Ok(inner));
    }

    #[test]
    fn e18_beacons_are_received_but_payloads_cannot_send_them() {
        let k = |s| Key::new(s).expect("valid");
        assert_eq!(FILTERS, [APP_FILTER, DISC_FILTER]);
        assert!(FILTERS.iter().any(|f| f.matches(k("disc/cam/jpeg"))), "received");
        assert!(!goes_over_air(k("disc/cam/jpeg")), "a payload cannot forge a beacon");
        assert!(!goes_over_air(k("disc")) && !carries_routes(k("disc/cam/jpeg")));
        assert!(is_beacon(k("disc/cam/jpeg")) && is_beacon(k("disc")));
        assert!(!is_beacon(k("discover/x")) && !is_beacon(k("app/disc")));
        assert!(!FILTERS.iter().any(|f| f.matches(k("discover/x"))), "segment-wise prefix");
        assert!(
            !FILTERS.iter().any(|f| f.matches(k("sys/info"))),
            "sys/ never admitted"
        );
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
