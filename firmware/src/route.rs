//! The firmware side of the ABI 4 DMA routes (`wasm_abi::route`): every
//! decision the route shell makes, pure and host tested.
//!
//! A payload configures a route; the firmware runs it without the frame
//! bytes ever passing through guest memory:
//!
//! * [`source`] — a **source route** ([`SourceRoute`]) paces one frame per
//!   period and decides, chunk by chunk, what to publish next;
//!   [`source::write_frame`] writes the ESP-NOW frame for one chunk (radio
//!   header, key, chunk header) and generates the chunk's pixels of the
//!   test pattern in place, so there is no frame buffer and no copy.
//! * [`sources`] — the [`SourceTable`] the radio task polls for due chunks
//!   (round robin over running routes) and folds broadcast results into.
//! * [`sink`] — a **sink route** ([`SinkRoute`]) runs each received message
//!   through the stream [`Sink`](middleware::stream::Sink) and returns the
//!   [`sink::transfers`] to make on the SPI bus: the preamble segments (each
//!   with its D/C level) at a frame start, then the chunk data with D/C high.
//! * [`table`] — the payload's [`Routes`]: handles `0..MAX_ROUTES`, the key
//!   of each route, and which bus and D/C pin a sink route owns.
//!
//! Every step yields a [`Report`] (frames completed and lost) that becomes
//! the coalescing `RouteFrames` / `RouteDropped` events ([`Report::events`]);
//! a route the firmware gives up on raises `RouteStopped`
//! ([`stopped_event`]).

pub mod sink;
pub mod source;
pub mod sources;
pub mod table;

pub use sink::SinkRoute;
pub use source::SourceRoute;
pub use sources::SourceTable;
pub use table::Routes;

use middleware::stream::{ESPNOW_MAX_PAYLOAD, HEADER_LEN};
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::events::{Event, EventKind};

/// Most data bytes per chunk a source route can send: a chunk header and
/// its data must fit one ESP-NOW payload (179 − 12 = 167 bytes). The ABI
/// allows up to 500; larger configs are `Unsupported` on this firmware.
pub const AIR_CHUNK_DATA: usize = ESPNOW_MAX_PAYLOAD - HEADER_LEN;

/// What one route step completed and lost, for its events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Frames completed (published, or fully sent to the bus).
    pub frames: u32,
    /// Frames lost (skipped or abandoned).
    pub dropped: u32,
}

impl Report {
    /// Nothing to report.
    pub const NONE: Report = Report { frames: 0, dropped: 0 };

    /// The `RouteFrames` and `RouteDropped` events for route `handle`
    /// (each only when its count is non-zero, values saturated to
    /// `i32::MAX`).
    pub fn events(self, handle: u8, time_us: u32) -> [Option<Event>; 2] {
        let event = |kind, n: u32| {
            (n > 0).then(|| Event {
                kind,
                source: i32::from(handle),
                value: i32::try_from(n).unwrap_or(i32::MAX),
                time_us,
            })
        };
        [event(EventKind::RouteFrames, self.frames), event(EventKind::RouteDropped, self.dropped)]
    }
}

/// The `RouteStopped` event for route `handle` given up after `fault`.
pub fn stopped_event(handle: u8, fault: AbiError, time_us: u32) -> Event {
    Event { kind: EventKind::RouteStopped, source: i32::from(handle), value: fault.code(), time_us }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r11_r12_reports_become_route_events() {
        assert_eq!(Report::NONE.events(1, 5), [None, None]);
        let [f, d] = Report { frames: 2, dropped: u32::MAX }.events(3, 9);
        assert_eq!(f.map(|e| e.to_args()), Some([EventKind::RouteFrames as i32, 3, 2, 9]));
        assert_eq!(d.map(|e| e.to_args()), Some([EventKind::RouteDropped as i32, 3, i32::MAX, 9]));
        let s = stopped_event(2, AbiError::Transport, 1);
        assert_eq!(s.to_args(), [EventKind::RouteStopped as i32, 2, AbiError::Transport.code(), 1]);
        assert_eq!(AIR_CHUNK_DATA, 167);
    }
}
