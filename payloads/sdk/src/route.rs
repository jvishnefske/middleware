//! DMA routes (ABI 4): frame streams between a pub/sub key and a host data
//! source or an SPI display, without the bytes passing through the payload.
//!
//! The payload only sets a route up; the host moves every frame:
//!
//! * [`Route::source`] binds a host data source ([`SourceKind`]: the
//!   synthetic [`SourceKind::TestPattern`], or the board's camera) to a key.
//!   Every frame period the host captures a frame into its own DMA-capable
//!   memory and publishes it on the key as chunks (the
//!   [`middleware::stream`] wire format).
//! * [`Route::sink`] binds a key to an SPI bus the payload opened with
//!   [`Spi::open`]. The payload initialises the panel itself first (with
//!   [`Spi::transfer`] and a D/C [`Output`](crate::gpio::Output)); then, for
//!   every frame received on the key, the host sends the [`Preamble`]
//!   (e.g. [`Preamble::mipi_window`]) and DMAs each in-order chunk to the
//!   bus with D/C high. Out-of-order, missing or repeated chunks are dropped,
//!   never drawn in the wrong place ([`middleware::stream::sink`]).
//!
//! While a sink route runs the host owns its bus and D/C pin (the payload's
//! own transfers on that bus are `Busy`) and consumes its key's messages
//! (they do not reach [`Payload::on_message`](crate::Payload::on_message)).
//! Progress arrives in [`Payload::on_irq`](crate::Payload::on_irq) as
//! [`Event::RouteFrames`], [`Event::RouteDropped`] and
//! [`Event::RouteStopped`]; [`Route::on_event`] picks out one route's.
//!
//! ```ignore
//! use mw_payload::route::{Preamble, Route, SinkConfig};
//!
//! // GC9A01 already initialised over `spi`, D/C on GPIO 13: show a
//! // 120×120 stream in the centre of the 240×240 panel.
//! let window = Preamble::mipi_window(60, 60, 179, 179)?;
//! let cfg = SinkConfig::new(13, 120 * 120 * 2)?.with_preamble(window);
//! let route = Route::sink("app/cam/frame", &spi, &cfg)?;
//! ```
//!
//! Configs are validated here with the ABI's own rules before the host is
//! called, so a bad geometry fails with [`AbiError::InvalidArgument`] even
//! without a host.

use crate::irq::Event;
use crate::spi::Spi;
use crate::{AbiError, check, len_i32, ptr_i32, sys};
pub use middleware::stream::{ESPNOW_CHUNK_DATA, MAX_CHUNK_DATA, MAX_FRAME_BYTES};
pub use middleware::wasm_abi::route::{
    MAX_DIM, MAX_PREAMBLE, MAX_ROUTES, MAX_SEGMENTS, MAX_SINK_CFG_LEN, MIN_PERIOD_MS, PixelFormat,
    Preamble, RouteStat, Segment, SinkConfig, SourceConfig, SourceKind, route_handle,
};

/// What happened to a route, from [`Route::on_event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteEvent {
    /// This many frames completed.
    Frames(u32),
    /// This many frames were lost.
    Dropped(u32),
    /// The route stopped by itself with this fault; the [`Route`] is dead.
    Stopped(AbiError),
}

/// A running route. Dropping it does *not* stop it; call [`Route::stop`].
#[derive(Debug, PartialEq, Eq)]
pub struct Route {
    handle: u8,
}

impl Route {
    /// Parses the host's handle result.
    fn started(code: i32) -> Result<Self, AbiError> {
        let handle = route_handle(check(code)?).map_err(|_| AbiError::Unsupported)?;
        Ok(Route { handle })
    }

    /// Starts a source route publishing frames from `source` on `key`.
    ///
    /// `InvalidArgument` (before calling the host) if `cfg` breaks a rule of
    /// [`SourceConfig::validated`]; from the host, `InvalidKey` for a
    /// reserved or malformed key, `Unsupported` for a source the board
    /// lacks, `NoResources` when [`MAX_ROUTES`] are running.
    pub fn source(source: SourceKind, key: &str, cfg: &SourceConfig) -> Result<Self, AbiError> {
        let cfg = cfg.validated()?.encode();
        Self::started(sys::route_source(
            source as i32,
            ptr_i32(key.as_bytes()),
            len_i32(key.len())?,
            ptr_i32(&cfg),
            len_i32(cfg.len())?,
        ))
    }

    /// Starts a sink route sending the frames received on `key` to `spi`'s
    /// bus.
    ///
    /// From the host: `InvalidKey` for a reserved or malformed key, `Busy`
    /// if the bus is not open, `Unsupported` for a D/C pin the board
    /// reserves, `NoResources` when [`MAX_ROUTES`] are running.
    pub fn sink(key: &str, spi: &Spi, cfg: &SinkConfig) -> Result<Self, AbiError> {
        let mut buf = [0u8; MAX_SINK_CFG_LEN];
        let n = cfg.encode(&mut buf);
        let bytes = buf.get(..n).ok_or(AbiError::InvalidArgument)?;
        Self::started(sys::route_sink(
            ptr_i32(key.as_bytes()),
            len_i32(key.len())?,
            i32::from(spi.config().abi().bus),
            ptr_i32(bytes),
            len_i32(bytes.len())?,
        ))
    }

    /// The handle, as reported in route events.
    pub const fn handle(&self) -> u8 {
        self.handle
    }

    /// One of the route's counters (saturating at `i32::MAX`).
    pub fn stat(&self, stat: RouteStat) -> Result<u32, AbiError> {
        check(sys::route_stat(i32::from(self.handle), stat as i32)).map(|n| n as u32)
    }

    /// Stops the route and discards its pending events. `Busy` if it had
    /// already stopped by itself.
    pub fn stop(self) -> Result<(), AbiError> {
        check(sys::route_stop(i32::from(self.handle))).map(|_| ())
    }

    /// This route's view of `event`, or `None` for any other event.
    pub fn on_event(&self, event: &Event) -> Option<RouteEvent> {
        match *event {
            Event::RouteFrames { route, frames, .. } if route == self.handle => {
                Some(RouteEvent::Frames(frames))
            }
            Event::RouteDropped { route, frames, .. } if route == self.handle => {
                Some(RouteEvent::Dropped(frames))
            }
            Event::RouteStopped { route, error, .. } if route == self.handle => {
                Some(RouteEvent::Stopped(error))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spi::SpiConfig;

    fn spi() -> Spi {
        let cfg = SpiConfig::builder(12)
            .mosi(11)
            .cs(10)
            .build()
            .expect("valid");
        Spi::for_test(cfg)
    }

    #[test]
    fn source_validates_before_the_host() {
        let mut bad = SourceConfig::new(120, 120, 100).expect("valid");
        bad.period_ms = 1;
        assert_eq!(
            Route::source(SourceKind::TestPattern, "app/cam/frame", &bad),
            Err(AbiError::InvalidArgument)
        );
        let ok = SourceConfig::new(120, 120, 100).expect("valid");
        assert_eq!(
            Route::source(SourceKind::TestPattern, "app/cam/frame", &ok),
            Err(AbiError::Unsupported),
            "host stub"
        );
    }

    #[test]
    fn sink_reaches_the_host_stub() {
        let cfg = SinkConfig::new(13, 120 * 120 * 2)
            .expect("valid")
            .with_preamble(Preamble::mipi_window(60, 60, 179, 179).expect("fits"));
        assert_eq!(
            Route::sink("app/cam/frame", &spi(), &cfg),
            Err(AbiError::Unsupported)
        );
    }

    #[test]
    fn started_checks_the_handle() {
        assert_eq!(Route::started(3), Ok(Route { handle: 3 }));
        assert_eq!(Route::started(4), Err(AbiError::Unsupported));
        assert_eq!(
            Route::started(AbiError::NoResources.code()),
            Err(AbiError::NoResources)
        );
        let r = Route { handle: 1 };
        assert_eq!(r.stat(RouteStat::Frames), Err(AbiError::Unsupported));
        assert_eq!(r.stop(), Err(AbiError::Unsupported));
    }

    #[test]
    fn on_event_selects_this_route() {
        let r = Route { handle: 2 };
        let frames = Event::RouteFrames {
            route: 2,
            frames: 3,
            time_us: 0,
        };
        let dropped = Event::RouteDropped {
            route: 2,
            frames: 1,
            time_us: 0,
        };
        let stopped = Event::RouteStopped {
            route: 2,
            error: AbiError::Transport,
            time_us: 0,
        };
        let other = Event::RouteFrames {
            route: 1,
            frames: 3,
            time_us: 0,
        };
        assert_eq!(r.on_event(&frames), Some(RouteEvent::Frames(3)));
        assert_eq!(r.on_event(&dropped), Some(RouteEvent::Dropped(1)));
        assert_eq!(
            r.on_event(&stopped),
            Some(RouteEvent::Stopped(AbiError::Transport))
        );
        assert_eq!(r.on_event(&other), None);
        assert_eq!(r.handle(), 2);
    }
}
