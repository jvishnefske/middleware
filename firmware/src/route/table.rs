//! The payload's routes: handles, keys, and what each sink route owns.
//!
//! [`Routes`] holds up to `MAX_ROUTES` entries, one per handle. The device
//! consults it before every route import and on every received message:
//!
//! * [`Routes::add`] gives a new route the lowest free handle
//!   (`NoResources` when all are taken). A key longer than an ESP-NOW key
//!   is `Unsupported`. A sink route is `Busy` when another sink already
//!   owns its bus, its D/C pin or its key (one sink per key, so a message
//!   has exactly one destination).
//! * [`Routes::remove`] frees a handle (`Busy` if it is not running) and
//!   returns the entry, so the shell releases what it held.
//! * [`Routes::sink_for`] finds the sink route consuming a key: such
//!   messages go to the route, never to `on_message`.
//! * [`Routes::owns_bus`] / [`Routes::owns_pin`] guard the payload's SPI
//!   and GPIO imports: while a sink route runs, its bus and D/C pin are
//!   `Busy` to the payload.
//! * [`Routes::with_sink`] stores a sink's next state after a message.

use middleware::Key;
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::route::MAX_ROUTES;

use super::sink::SinkRoute;
use super::sources::RouteKey;

/// What a route is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A source route; its running state is in the
    /// [`SourceTable`](super::SourceTable).
    Source,
    /// A sink route on SPI bus `bus`.
    Sink {
        /// The bus it owns.
        bus: u8,
        /// Its state.
        route: SinkRoute,
    },
}

/// One route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// The key it publishes or consumes.
    pub key: RouteKey,
    /// Source or sink.
    pub kind: Kind,
}

impl Entry {
    /// The D/C pin a sink route drives.
    pub const fn dc_pin(&self) -> Option<u8> {
        match &self.kind {
            Kind::Sink { route, .. } => Some(route.config().dc_pin),
            Kind::Source => None,
        }
    }

    /// The bus a sink route owns.
    pub const fn bus(&self) -> Option<u8> {
        match self.kind {
            Kind::Sink { bus, .. } => Some(bus),
            Kind::Source => None,
        }
    }
}

/// The routes of one payload run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Routes {
    slots: [Option<Entry>; MAX_ROUTES],
}

impl Routes {
    /// No routes.
    pub const fn new() -> Self {
        Routes { slots: [None; MAX_ROUTES] }
    }

    fn entries(&self) -> impl Iterator<Item = (u8, &Entry)> {
        // index < MAX_ROUTES, so it fits u8.
        self.slots.iter().enumerate().filter_map(|(i, e)| e.as_ref().map(|e| (i as u8, e)))
    }

    /// The table with a new route; returns it and the route's handle.
    pub fn add(self, key: Key<'_>, kind: Kind) -> Result<(Self, u8), AbiError> {
        let key = RouteKey::from_key(key).map_err(|_| AbiError::Unsupported)?;
        if let Kind::Sink { bus, route } = &kind {
            let dc = route.config().dc_pin;
            let clash = self.entries().any(|(_, e)| {
                matches!(e.kind, Kind::Sink { .. })
                    && (e.bus() == Some(*bus) || e.dc_pin() == Some(dc) || e.key == key)
            });
            if clash {
                return Err(AbiError::Busy);
            }
        }
        let index = self.slots.iter().position(Option::is_none).ok_or(AbiError::NoResources)?;
        let mut next = self;
        let slot = next.slots.get_mut(index).ok_or(AbiError::NoResources)?;
        *slot = Some(Entry { key, kind });
        Ok((next, index as u8)) // index < MAX_ROUTES
    }

    /// The table without route `handle`, and its entry; `Busy` if it is not
    /// running.
    pub fn remove(self, handle: u8) -> Result<(Self, Entry), AbiError> {
        let mut next = self;
        let slot = next.slots.get_mut(usize::from(handle)).ok_or(AbiError::Busy)?;
        let entry = slot.take().ok_or(AbiError::Busy)?;
        Ok((next, entry))
    }

    /// Route `handle`, if it runs.
    pub fn get(&self, handle: u8) -> Option<&Entry> {
        self.slots.get(usize::from(handle)).and_then(Option::as_ref)
    }

    /// The sink route consuming `key`, with its state.
    pub fn sink_for(&self, key: Key<'_>) -> Option<(u8, SinkRoute)> {
        self.entries().find_map(|(h, e)| match e.kind {
            Kind::Sink { route, .. } if e.key == key => Some((h, route)),
            _ => None,
        })
    }

    /// The table with sink route `handle` in state `route` (unchanged if
    /// `handle` is not a sink route).
    pub fn with_sink(self, handle: u8, route: SinkRoute) -> Self {
        let mut next = self;
        if let Some(Some(Entry { kind: Kind::Sink { route: r, .. }, .. })) =
            next.slots.get_mut(usize::from(handle))
        {
            *r = route;
        }
        next
    }

    /// Whether a sink route owns SPI bus `bus`.
    pub fn owns_bus(&self, bus: u8) -> bool {
        self.entries().any(|(_, e)| e.bus() == Some(bus))
    }

    /// Whether a sink route drives `pin` as its D/C pin.
    pub fn owns_pin(&self, pin: u8) -> bool {
        self.entries().any(|(_, e)| e.dc_pin() == Some(pin))
    }

    /// Whether no route runs.
    pub fn is_empty(&self) -> bool {
        self.entries().next().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::route::SinkConfig;

    fn k(s: &str) -> Key<'_> {
        Key::new(s).expect("valid")
    }

    fn sink(bus: u8, dc: u8) -> Kind {
        Kind::Sink { bus, route: SinkRoute::new(SinkConfig::new(dc, 4).expect("valid")) }
    }

    #[test]
    fn r11_r12_handles_are_allocated_and_freed() {
        let r = Routes::new();
        assert!(r.is_empty());
        let (r, a) = r.add(k("app/a"), Kind::Source).expect("free");
        let (r, b) = r.add(k("app/b"), sink(0, 13)).expect("free");
        assert_eq!((a, b), (0, 1));
        let (r, _) = r.add(k("app/c"), Kind::Source).expect("free");
        let (r, _) = r.add(k("app/d"), Kind::Source).expect("free");
        assert_eq!(r.add(k("app/e"), Kind::Source), Err(AbiError::NoResources));
        let (r, e) = r.remove(a).expect("running");
        assert_eq!((e.key.as_str(), e.kind), ("app/a", Kind::Source));
        assert_eq!(r.remove(a), Err(AbiError::Busy), "not running");
        assert_eq!(r.remove(200), Err(AbiError::Busy));
        let (_, again) = r.add(k("app/a"), Kind::Source).expect("free");
        assert_eq!(again, 0, "lowest free handle");
    }

    #[test]
    fn r12_a_sink_owns_its_bus_pin_and_key() {
        let (r, h) = Routes::new().add(k("app/cam/frame"), sink(0, 13)).expect("free");
        assert!(r.owns_bus(0));
        assert!(!r.owns_bus(1));
        assert!(r.owns_pin(13));
        assert!(!r.owns_pin(14));
        assert_eq!(r.sink_for(k("app/cam/frame")).map(|(h, _)| h), Some(h));
        assert_eq!(r.sink_for(k("app/cam")), None);
        assert_eq!(r.add(k("app/x"), sink(0, 14)), Err(AbiError::Busy), "bus");
        assert_eq!(r.add(k("app/x"), sink(1, 13)), Err(AbiError::Busy), "pin");
        assert_eq!(r.add(k("app/cam/frame"), sink(1, 14)), Err(AbiError::Busy), "key");
        // A source may publish on a key a sink consumes (loopback).
        assert!(r.add(k("app/cam/frame"), Kind::Source).is_ok());
        let (r, e) = r.remove(h).expect("running");
        assert_eq!((e.bus(), e.dc_pin()), (Some(0), Some(13)));
        assert!(!r.owns_bus(0) && !r.owns_pin(13), "released");
        assert!(r.get(h).is_none());
    }

    #[test]
    fn r12_sink_state_is_stored_and_long_keys_are_refused() {
        let (r, h) = Routes::new().add(k("app/s"), sink(0, 13)).expect("free");
        let (_, before) = r.sink_for(k("app/s")).expect("sink");
        let (after, _) = before.receive(&[0xFF]);
        let r = r.with_sink(h, after);
        assert_eq!(r.sink_for(k("app/s")).map(|(_, s)| s.stats().chunks_dropped), Some(1));
        assert_eq!(r.with_sink(3, before), r, "no such sink");
        let long = "app/0123456789012345678901234567890123456789012345678901234567890";
        assert!(long.len() > middleware::espnow::MAX_KEY);
        assert_eq!(Routes::new().add(k(long), Kind::Source), Err(AbiError::Unsupported));
    }
}
