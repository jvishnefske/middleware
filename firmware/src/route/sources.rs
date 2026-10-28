//! The running source routes, as the radio task sees them.
//!
//! The controller starts and stops source routes; the radio task sends
//! their chunks. They meet in one [`SourceTable`] (the shell keeps it in a
//! critical-section mutex and never holds the lock across an await):
//!
//! * [`SourceTable::start`] / [`SourceTable::stop`] / [`SourceTable::clear`]
//!   (controller) fill and empty slots, indexed by route handle. Every start
//!   stamps the slot with a fresh *epoch*.
//! * [`SourceTable::due`] (radio task) picks the next chunk to send: the
//!   first route, scanning round robin from the one after the route served
//!   last, that has a chunk due now ([`Due::Send`] with a [`Job`] carrying
//!   everything [`super::source::write_frame`] needs); otherwise the
//!   shortest wait ([`Due::Wait`]); [`Due::Idle`] when nothing runs.
//! * [`SourceTable::finish`] (radio task) folds the broadcast result into
//!   the route and returns its [`Report`]. A job whose route was stopped or
//!   restarted meanwhile (epoch mismatch) changes nothing and reports
//!   nothing, so a stopped route never raises an event.
//! * [`SourceTable::stats`] backs `route_stat`.

use middleware::KeyBuf;
use middleware::espnow::MAX_KEY;
use middleware::stream::{ChunkPlan, FrameSource, RouteStats};
use middleware::wasm_abi::route::{MAX_ROUTES, SourceConfig};

use super::Report;
use super::source::{Next, SourceRoute};

/// A route key that fits one ESP-NOW frame.
pub type RouteKey = KeyBuf<MAX_KEY>;

/// One running source route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    key: RouteKey,
    route: SourceRoute,
    epoch: u32,
}

/// A chunk to send now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Route handle.
    pub handle: u8,
    /// The route's epoch when the job was taken.
    pub epoch: u32,
    /// The route's key.
    pub key: RouteKey,
    /// Source frame number.
    pub frame: u32,
    /// The route's config.
    pub cfg: SourceConfig,
    /// Where the chunk's bytes come from.
    pub source: FrameSource<'static>,
    /// The chunk.
    pub plan: ChunkPlan,
}

/// What the radio task does about source routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// Send this chunk now.
    Send(Job),
    /// Nothing for this many milliseconds (unless a route starts).
    Wait(u32),
    /// No source route runs.
    Idle,
}

/// The source routes of the running payload, by handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTable {
    slots: [Option<Slot>; MAX_ROUTES],
    /// Epoch of the latest start.
    epoch: u32,
    /// Slot served last, for round robin.
    last: u8,
}

impl Default for SourceTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceTable {
    /// No routes.
    pub const fn new() -> Self {
        SourceTable { slots: [None; MAX_ROUTES], epoch: 0, last: 0 }
    }

    /// The table with `route` running on `key` as handle `handle` (a
    /// handle outside `0..MAX_ROUTES` changes nothing).
    pub fn start(self, handle: u8, key: RouteKey, route: SourceRoute) -> Self {
        let epoch = self.epoch.wrapping_add(1);
        let mut next = SourceTable { epoch, ..self };
        if let Some(slot) = next.slots.get_mut(usize::from(handle)) {
            *slot = Some(Slot { key, route, epoch });
        }
        next
    }

    /// The table without route `handle`.
    pub fn stop(self, handle: u8) -> Self {
        let mut next = self;
        if let Some(slot) = next.slots.get_mut(usize::from(handle)) {
            *slot = None;
        }
        next
    }

    /// The table with no routes (payload stop); epochs keep counting.
    pub const fn clear(self) -> Self {
        SourceTable { slots: [None; MAX_ROUTES], ..self }
    }

    /// Whether route `handle` runs.
    pub fn running(&self, handle: u8) -> bool {
        matches!(self.slots.get(usize::from(handle)), Some(Some(_)))
    }

    /// The counters of route `handle`, if it runs.
    pub fn stats(&self, handle: u8) -> Option<RouteStats> {
        self.slots.get(usize::from(handle)).copied().flatten().map(|s| s.route.stats())
    }

    /// The next chunk to send at `now_ms`, round robin over the routes.
    pub fn due(self, now_ms: u32) -> (Self, Due) {
        let mut wait: Option<u32> = None;
        for i in 1..=MAX_ROUTES {
            let index = (usize::from(self.last) + i) % MAX_ROUTES;
            let Some(Some(slot)) = self.slots.get(index) else { continue };
            match slot.route.next(now_ms) {
                Next::Send(plan) => {
                    let handle = index as u8; // index < MAX_ROUTES
                    let job = Job {
                        handle,
                        epoch: slot.epoch,
                        key: slot.key,
                        frame: slot.route.frame(),
                        cfg: slot.route.config(),
                        source: slot.route.source(),
                        plan,
                    };
                    return (SourceTable { last: handle, ..self }, Due::Send(job));
                }
                Next::Wait(ms) => wait = Some(wait.map_or(ms, |w| w.min(ms))),
            }
        }
        (self, wait.map_or(Due::Idle, Due::Wait))
    }

    /// Folds the broadcast of `job`'s chunk (`ok`, at `now_ms`) into its
    /// route; nothing when the route stopped or restarted since.
    pub fn finish(self, handle: u8, epoch: u32, ok: bool, now_ms: u32) -> (Self, Report) {
        let mut next = self;
        let Some(Some(slot)) = next.slots.get_mut(usize::from(handle)) else {
            return (self, Report::NONE);
        };
        if slot.epoch != epoch {
            return (self, Report::NONE);
        }
        let (route, report) =
            if ok { slot.route.published(now_ms) } else { slot.route.failed(now_ms) };
        slot.route = route;
        (next, report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::Key;

    fn key(s: &str) -> RouteKey {
        RouteKey::from_key(Key::new(s).expect("valid")).expect("fits")
    }

    fn route(period: u16, now: u32) -> SourceRoute {
        SourceRoute::new(SourceConfig::new(2, 2, period).expect("valid"), now).expect("valid")
    }

    fn job(d: Due) -> Job {
        match d {
            Due::Send(j) => j,
            other => panic!("expected a job, got {other:?}"),
        }
    }

    #[test]
    fn r11_idle_wait_and_send() {
        let t = SourceTable::new();
        assert_eq!(t.due(0).1, Due::Idle);
        let t = t.start(2, key("app/a"), route(100, 50));
        assert_eq!(t.due(0).1, Due::Wait(50));
        let (t, d) = t.due(50);
        let j = job(d);
        assert_eq!((j.handle, j.key.as_str(), j.frame, j.plan.range.clone()), (2, "app/a", 0, 0..8));
        // A 2×2 frame is one chunk: finishing it completes the frame.
        let (t, rep) = t.finish(j.handle, j.epoch, true, 51);
        assert_eq!(rep, Report { frames: 1, dropped: 0 });
        assert_eq!(t.stats(2).map(|s| s.frames), Some(1));
        assert_eq!(t.due(51).1, Due::Wait(99));
        assert!(t.running(2));
        assert!(!t.running(0));
        assert_eq!(t.stats(0), None);
        assert_eq!(t.stats(200), None);
    }

    #[test]
    fn r11_routes_take_turns() {
        let t = SourceTable::new()
            .start(0, key("app/a"), route(10, 0))
            .start(3, key("app/b"), route(10, 0));
        let (t, d) = t.due(0);
        let a = job(d);
        let t = t.finish(a.handle, a.epoch, true, 0).0;
        // Both are due (route 0 again late): the other one goes first.
        let (_, d) = t.due(20);
        assert_eq!(job(d).handle, if a.handle == 0 { 3 } else { 0 });
    }

    #[test]
    fn r11_stopped_or_restarted_routes_ignore_late_results() {
        let t = SourceTable::new().start(1, key("app/a"), route(10, 0));
        let (t, d) = t.due(0);
        let j = job(d);
        let stopped = t.stop(1);
        assert_eq!(stopped.finish(1, j.epoch, true, 1), (stopped, Report::NONE));
        let restarted = stopped.start(1, key("app/a"), route(10, 0));
        let (after, rep) = restarted.finish(1, j.epoch, false, 1);
        assert_eq!((after, rep), (restarted, Report::NONE));
        assert_eq!(restarted.stats(1), Some(RouteStats::default()));
        assert_eq!(restarted.clear().due(0).1, Due::Idle);
        // A failed broadcast drops the frame.
        let (_, rep) = restarted.finish(1, restarted.epoch, false, 1);
        assert_eq!(rep, Report { frames: 0, dropped: 1 });
        // Out-of-range handles change nothing.
        assert_eq!(SourceTable::new().start(9, key("app/a"), route(10, 0)).due(0).1, Due::Idle);
    }
}
