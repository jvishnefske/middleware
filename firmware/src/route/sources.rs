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
//!   everything [`super::source::write_frame`] needs) or a camera frame to
//!   capture ([`Due::Capture`], reported back with
//!   [`SourceTable::captured`]); otherwise the shortest wait
//!   ([`Due::Wait`]); [`Due::Idle`] when nothing runs.
//! * [`SourceTable::finish`] (radio task) folds the broadcast result into
//!   the route and returns its [`Report`]. A job whose route was stopped or
//!   restarted meanwhile (epoch mismatch) changes nothing and reports
//!   nothing, so a stopped route never raises an event.
//! * [`SourceTable::stats`] backs `route_stat`.

use middleware::KeyBuf;
use middleware::espnow::MAX_KEY;
use middleware::stream::{ChunkPlan, RouteStats};
use middleware::wasm_abi::route::{MAX_ROUTES, SourceConfig};

use super::Report;
use super::source::{CaptureFault, Feed, Next, SourceRoute};

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
    /// Where the chunk's bytes come from (for [`Feed::Camera`], the
    /// shell's frame buffer).
    pub feed: Feed,
    /// The chunk.
    pub plan: ChunkPlan,
}

/// A camera frame to capture now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capture {
    /// Route handle.
    pub handle: u8,
    /// The route's epoch when the capture was asked for.
    pub epoch: u32,
    /// The route's config (frame size, period).
    pub cfg: SourceConfig,
}

/// What the radio task does about source routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// Capture a camera frame for this route, then report it with
    /// [`SourceTable::captured`].
    Capture(Capture),
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
                Next::Capture => {
                    let handle = index as u8; // index < MAX_ROUTES
                    let capture = Capture { handle, epoch: slot.epoch, cfg: slot.route.config() };
                    return (SourceTable { last: handle, ..self }, Due::Capture(capture));
                }
                Next::Send(plan) => {
                    let handle = index as u8; // index < MAX_ROUTES
                    let job = Job {
                        handle,
                        epoch: slot.epoch,
                        key: slot.key,
                        frame: slot.route.frame(),
                        cfg: slot.route.config(),
                        feed: slot.route.feed(),
                        plan,
                    };
                    return (SourceTable { last: handle, ..self }, Due::Send(job));
                }
                Next::Wait(ms) => wait = Some(wait.map_or(ms, |w| w.min(ms))),
            }
        }
        (self, wait.map_or(Due::Idle, Due::Wait))
    }

    /// Folds the result of `capture` (a frame of that many bytes in the
    /// shell's frame buffer, or why there is none) into its route; nothing
    /// when the route stopped or restarted since.
    pub fn captured(self, capture: Capture, result: Result<u32, CaptureFault>, now_ms: u32) -> (Self, Report) {
        let mut next = self;
        let Some(Some(slot)) = next.slots.get_mut(usize::from(capture.handle)) else {
            return (self, Report::NONE);
        };
        if slot.epoch != capture.epoch {
            return (self, Report::NONE);
        }
        let (route, report) = slot.route.captured(now_ms, result);
        slot.route = route;
        (next, report)
    }

    /// Whether a running route captures from the camera.
    pub fn has_camera(&self) -> bool {
        self.camera_key().is_some()
    }

    /// The key of the first running route that captures from the camera
    /// (what the camera beacon announces, `esp_base::radio::disc`).
    pub fn camera_key(&self) -> Option<&RouteKey> {
        self.slots.iter().flatten().find(|s| matches!(s.route.feed(), Feed::Camera { .. })).map(|s| &s.key)
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
    fn r15_camera_routes_ask_for_a_capture_then_send_its_chunks() {
        let cam = SourceRoute::camera(SourceConfig::jpeg(240, 240, 500).expect("valid"), 0).expect("valid");
        let t = SourceTable::new().start(1, key("app/cam/jpeg"), cam);
        assert!(t.has_camera());
        assert_eq!(t.camera_key().map(|k| middleware::AsKey::as_key(k).as_str()), Some("app/cam/jpeg"));
        assert!(!SourceTable::new().start(0, key("app/a"), route(10, 0)).has_camera());
        assert_eq!(SourceTable::new().start(0, key("app/a"), route(10, 0)).camera_key(), None);
        let (t, d) = t.due(0);
        let Due::Capture(c) = d else { panic!("capture, got {d:?}") };
        assert_eq!((c.handle, c.cfg.width), (1, 240));
        // Until the result arrives the table asks again (the shell captures
        // one frame at a time).
        assert_eq!(t.due(1).1, Due::Capture(c));
        let (t, rep) = t.captured(c, Ok(200), 50);
        assert_eq!(rep, Report::NONE);
        let (t, d) = t.due(50);
        let j = job(d);
        assert_eq!(j.feed, Feed::Camera { len: 200 });
        assert_eq!(j.plan.header.total, 200);
        // A stale capture result (route restarted) is ignored.
        let restarted = t.stop(1).start(1, key("app/cam/jpeg"), cam);
        assert_eq!(restarted.captured(c, Err(CaptureFault::Timeout), 60), (restarted, Report::NONE));
        // A failure drops the frame.
        let (_, rep) = t.stop(1).start(1, key("app/cam/jpeg"), cam).captured(
            Capture { epoch: restarted.epoch, ..c },
            Err(CaptureFault::Overflow),
            60,
        );
        assert_eq!(rep, Report { frames: 0, dropped: 1 });
        assert_eq!(t.captured(Capture { handle: 9, ..c }, Ok(1), 0).1, Report::NONE);
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
