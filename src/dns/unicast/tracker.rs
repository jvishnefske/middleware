//! [`Tracker`]: the endpoints of one browsed kind as a pure value, the
//! [`Diff`] each new resolution makes to them, and when to resolve again
//! ([`Schedule`]).
//!
//! Unicast DNS has no announcements or goodbyes: the browser polls, and the
//! tracker turns successive answers into events. Every transition takes the
//! tracker by value with the current time (milliseconds on any monotonic
//! clock) and returns the new tracker, plus the [`Diff`] for
//! [`Tracker::update`].
//!
//! # Events from answers
//!
//! [`Tracker::update`] compares the resolved services with those shown
//! before, by instance:
//!
//! * a new instance is [`Event::Up`];
//! * a known instance whose node, endpoint or TXT changed is `Up` again;
//! * a known, unchanged instance is silent — a caching resolver counts the
//!   TTL down between polls, so the TTL alone is no change — until 80 %
//!   ([`REFRESH_PERCENT`]) of the TTL it was last shown with has passed;
//!   then it is `Up` again, a refresh, so that a [`Directory`] fed with the
//!   events (which expires records at their TTL) keeps it;
//! * an instance no longer resolved is [`Event::Down`].
//!
//! Downs come before Ups. A resolved TTL of 0 (a resolver saying "do not
//! cache") is shown as 1 s, because an `Up` with TTL 0 means a goodbye. The
//! first of several services with one instance name is kept; services that
//! do not fit a [`ServiceRecord`] (a host name over 64 bytes, see
//! [`directory`](crate::discovery::directory)) or beyond `N` are not
//! tracked and are counted in [`Diff::rejected`].
//!
//! # When to resolve again
//!
//! After an answer with services, at 80 % of the smallest resolved TTL, the
//! rule of [`Directory::next_refresh`]; after an empty answer (NXDOMAIN or
//! no records) and after a failure, at fixed intervals. Each delay is
//! clamped to the [`Schedule`]'s `min..=max`, so a TTL of 0 does not poll
//! in a loop and a day-long TTL still notices a pod going away.
//! [`Tracker::new`] is due at once.
//!
//! [`Directory`]: crate::discovery::Directory
//! [`Directory::next_refresh`]: crate::discovery::Directory::next_refresh

use heapless::{Deque, Vec};

use crate::discovery::directory::REFRESH_PERCENT;
use crate::discovery::{Event, Service, ServiceRecord};

/// When to resolve again; see the module docs. All times in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    min_ms: u64,
    max_ms: u64,
    empty_ms: u64,
    error_ms: u64,
}

impl Default for Schedule {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Schedule {
    /// Between 1 s and 5 min after an answer, 5 s after an empty one, 2 s
    /// after a failure.
    pub const DEFAULT: Schedule = Schedule {
        min_ms: 1_000,
        max_ms: 300_000,
        empty_ms: 5_000,
        error_ms: 2_000,
    };

    /// Delays clamped to `min_ms..=max_ms` (swapped if given the wrong way
    /// round), keeping the empty and error intervals.
    pub const fn new(min_ms: u64, max_ms: u64) -> Self {
        let (min_ms, max_ms) = if min_ms <= max_ms {
            (min_ms, max_ms)
        } else {
            (max_ms, min_ms)
        };
        Schedule {
            min_ms,
            max_ms,
            ..Self::DEFAULT
        }
    }

    /// The schedule with `ms` after an empty answer.
    pub const fn empty_interval(self, ms: u64) -> Self {
        Schedule {
            empty_ms: ms,
            ..self
        }
    }

    /// The schedule with `ms` after a failure.
    pub const fn error_interval(self, ms: u64) -> Self {
        Schedule {
            error_ms: ms,
            ..self
        }
    }

    const fn clamp(&self, ms: u64) -> u64 {
        if ms < self.min_ms {
            self.min_ms
        } else if ms > self.max_ms {
            self.max_ms
        } else {
            ms
        }
    }

    /// Delay after an answer whose smallest TTL is `ttl_s`: 80 % of it,
    /// clamped.
    pub const fn after_answer(&self, ttl_s: u32) -> u64 {
        self.clamp(ttl_s as u64 * 10 * REFRESH_PERCENT)
    }

    /// Delay after an empty answer, clamped.
    pub const fn after_empty(&self) -> u64 {
        self.clamp(self.empty_ms)
    }

    /// Delay after a failure, clamped.
    pub const fn after_error(&self) -> u64 {
        self.clamp(self.error_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    /// What the last `Up` reported.
    shown: ServiceRecord,
    /// When it was reported.
    shown_ms: u64,
}

impl Tracked {
    fn refresh_due(&self, now_ms: u64) -> bool {
        let after = u64::from(self.shown.ttl_s()) * 10 * REFRESH_PERCENT;
        now_ms >= self.shown_ms.saturating_add(after)
    }
}

/// True if `a` and `b` differ at most in TTL.
fn same_but_ttl(a: &ServiceRecord, b: &ServiceRecord) -> bool {
    let (a, b) = (a.service(), b.service());
    Service { ttl_s: 0, ..a } == Service { ttl_s: 0, ..b }
}

/// What a [`Tracker::update`] changed, in delivery order (Downs, then
/// Ups), as a queue: [`Diff::front`] then [`Diff::advance`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff<const N: usize> {
    down: Deque<ServiceRecord, N>,
    up: Deque<ServiceRecord, N>,
    rejected: usize,
}

impl<const N: usize> Diff<N> {
    /// The next event, if any.
    pub fn front(&self) -> Option<Event<'_>> {
        match self.down.front() {
            Some(r) => {
                let s = r.service();
                Some(Event::Down {
                    kind: s.kind,
                    instance: s.instance,
                })
            }
            None => self.up.front().map(|r| Event::Up(r.service())),
        }
    }

    /// The queue without its front event.
    pub fn advance(self) -> Self {
        let (mut down, mut up) = (self.down, self.up);
        if down.pop_front().is_none() {
            up.pop_front();
        }
        Diff { down, up, ..self }
    }

    /// Events left.
    pub fn len(&self) -> usize {
        self.down.len() + self.up.len()
    }

    /// True when no event is left.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolved services that were not tracked (see the module docs).
    pub const fn rejected(&self) -> usize {
        self.rejected
    }
}

/// The endpoints of one kind, as last shown; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tracker<const N: usize> {
    entries: Vec<Tracked, N>,
    due_ms: u64,
}

impl<const N: usize> Tracker<N> {
    /// Most endpoints tracked.
    pub const CAPACITY: usize = N;

    /// No endpoints, due at once.
    pub const fn new() -> Self {
        Tracker {
            entries: Vec::new(),
            due_ms: 0,
        }
    }

    /// Applies a successful resolution at `now_ms` that found `resolved`
    /// (empty for NXDOMAIN or no records); see the module docs.
    pub fn update(
        self,
        resolved: &[Service<'_>],
        now_ms: u64,
        schedule: &Schedule,
    ) -> (Self, Diff<N>) {
        let mut entries: Vec<Tracked, N> = Vec::new();
        let mut diff = Diff::<N>::default();
        let mut min_ttl: Option<u32> = None;
        // Bounded by the slice.
        for s in resolved {
            let s = Service {
                ttl_s: s.ttl_s.max(1),
                ..*s
            };
            if entries.iter().any(|t| t.shown.is(s.kind, s.instance)) {
                continue;
            }
            let Some(record) = ServiceRecord::new(&s).filter(|_| !entries.is_full()) else {
                diff.rejected += 1;
                continue;
            };
            min_ttl = Some(min_ttl.map_or(s.ttl_s, |t| t.min(s.ttl_s)));
            let shown = self
                .entries
                .iter()
                .find(|t| t.shown.is(s.kind, s.instance))
                .filter(|t| same_but_ttl(&t.shown, &record) && !t.refresh_due(now_ms));
            let tracked = match shown {
                Some(t) => t.clone(),
                None => {
                    // Capacity: at most N entries, so at most N Ups.
                    let _ = diff.up.push_back(record.clone());
                    Tracked {
                        shown: record,
                        shown_ms: now_ms,
                    }
                }
            };
            // Cannot fail: checked `is_full` above.
            let _ = entries.push(tracked);
        }
        for old in &self.entries {
            let s = old.shown.service();
            if !entries.iter().any(|t| t.shown.is(s.kind, s.instance)) {
                // At most N old entries, so at most N Downs.
                let _ = diff.down.push_back(old.shown.clone());
            }
        }
        let delay = match min_ttl {
            Some(ttl) => schedule.after_answer(ttl),
            None => schedule.after_empty(),
        };
        let tracker = Tracker {
            entries,
            due_ms: now_ms.saturating_add(delay),
        };
        (tracker, diff)
    }

    /// Records a failed resolution at `now_ms`: the endpoints stay as
    /// shown, and the next attempt is due after the error interval.
    pub fn failed(self, now_ms: u64, schedule: &Schedule) -> Self {
        Tracker {
            due_ms: now_ms.saturating_add(schedule.after_error()),
            ..self
        }
    }

    /// When to resolve next.
    pub const fn due_ms(&self) -> u64 {
        self.due_ms
    }

    /// The endpoints as last shown.
    pub fn iter(&self) -> impl Iterator<Item = Service<'_>> {
        self.entries.iter().map(|t| t.shown.service())
    }

    /// Endpoints tracked.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no endpoint is tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::discovery::{Directory, Endpoint, ServiceKind};
    use std::vec::Vec as StdVec;

    const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");

    fn pod(instance: &'static str, last: u8, ttl_s: u32) -> Service<'static> {
        Service {
            kind: CAM,
            instance,
            node: None,
            endpoint: Endpoint::Socket(std::format!("10.0.0.{last}:80").parse().unwrap()),
            txt: b"",
            ttl_s,
        }
    }

    fn drain<const N: usize>(diff: Diff<N>) -> StdVec<std::string::String> {
        let mut out = StdVec::new();
        let mut diff = diff;
        for _ in 0..2 * N {
            let Some(e) = diff.front() else { break };
            out.push(match e {
                Event::Up(s) => std::format!("up {} {}", s.instance, s.ttl_s),
                Event::Down { instance, .. } => std::format!("down {instance}"),
            });
            diff = diff.advance();
        }
        out
    }

    const S: Schedule = Schedule::new(100, 60_000)
        .empty_interval(5_000)
        .error_interval(2_000);

    #[test]
    fn answers_become_up_and_down_diffs() {
        let t = Tracker::<4>::new();
        assert_eq!(t.due_ms(), 0, "due at once");
        let (t, d) = t.update(&[pod("a", 1, 30), pod("b", 2, 30)], 1_000, &S);
        assert_eq!(drain(d), ["up a 30", "up b 30"]);
        // Unchanged, TTL counting down in a cache: silent.
        let (t, d) = t.update(&[pod("b", 2, 20), pod("a", 1, 21)], 11_000, &S);
        assert!(d.is_empty());
        // 2 → 3 pods, one moved: Up for the new and the moved.
        let (t, d) = t.update(
            &[pod("a", 1, 30), pod("b", 9, 30), pod("c", 3, 30)],
            12_000,
            &S,
        );
        assert_eq!(drain(d), ["up b 30", "up c 30"]);
        assert_eq!(t.len(), 3);
        // 3 → 1: Downs, in tracked order, before any Up.
        let (t, d) = t.update(&[pod("c", 3, 30), pod("d", 4, 30)], 13_000, &S);
        assert_eq!(drain(d), ["down a", "down b", "up d 30"]);
        // NXDOMAIN: everything goes.
        let (t, d) = t.update(&[], 14_000, &S);
        assert_eq!(drain(d), ["down c", "down d"]);
        assert!(t.is_empty());
        assert_eq!(t.due_ms(), 14_000 + 5_000);
    }

    #[test]
    fn duplicates_ttl_zero_and_capacity() {
        let (t, d) = Tracker::<2>::new().update(
            &[
                pod("a", 1, 0),
                pod("a", 2, 30),
                pod("b", 2, 30),
                pod("c", 3, 30),
            ],
            0,
            &S,
        );
        assert_eq!(
            d.rejected(),
            1,
            "c beyond capacity; the second a is a duplicate"
        );
        assert_eq!(drain(d), ["up a 1", "up b 30"]);
        assert_eq!(t.iter().map(|s| s.ttl_s).collect::<StdVec<_>>(), [1, 30]);
        let long = "h".repeat(65);
        let host = Service {
            endpoint: Endpoint::Host {
                name: &long,
                port: 80,
            },
            ..pod("x", 1, 30)
        };
        let (_, d) = Tracker::<2>::new().update(&[host], 0, &S);
        assert_eq!((d.rejected(), d.len()), (1, 0));
    }

    #[test]
    fn re_resolution_is_due_at_80_percent_of_the_ttl_clamped() {
        let (t, _) = Tracker::<4>::new().update(&[pod("a", 1, 10), pod("b", 2, 60)], 1_000, &S);
        assert_eq!(t.due_ms(), 1_000 + 8_000, "80 % of the smallest TTL");
        // The same rule as Directory::next_refresh.
        let (dir, _) = Directory::<4>::new().observe(&Event::Up(pod("a", 1, 10)), 1_000);
        assert_eq!(dir.next_refresh(0), Some(t.due_ms()));
        let (t, _) = t.update(&[pod("a", 1, 0)], 2_000, &S);
        assert_eq!(t.due_ms(), 2_000 + 800, "TTL 0 is shown as 1 s");
        let (t, _) = t.update(&[pod("a", 1, 86_400)], 3_000, &S);
        assert_eq!(t.due_ms(), 3_000 + 60_000, "clamped to max");
        let t = t.failed(4_000, &S);
        assert_eq!((t.due_ms(), t.len()), (4_000 + 2_000, 1), "kept on failure");
        let tight = Schedule::new(500, 100)
            .empty_interval(10)
            .error_interval(1_000_000);
        assert_eq!(
            (
                tight.after_answer(1),
                tight.after_empty(),
                tight.after_error()
            ),
            (500, 100, 500)
        );
        assert_eq!(Schedule::default(), Schedule::DEFAULT);
    }

    #[test]
    fn unchanged_instances_are_refreshed_before_a_directory_expires_them() {
        let (t, d) = Tracker::<2>::new().update(&[pod("a", 1, 10)], 0, &S);
        let (mut dir, _) = Directory::<2>::new().observe(&d.front().unwrap(), 0);
        // Polls every 5 s with a cache counting the TTL down, then back up.
        let mut t = t;
        for (now, ttl, refreshed) in [
            (5_000, 5, false),
            (7_999, 2, false),
            (8_000, 10, true),
            (12_000, 6, false),
            (16_000, 10, true),
        ] {
            let (next, d) = t.update(&[pod("a", 1, ttl)], now, &S);
            t = next;
            assert_eq!(d.len(), usize::from(refreshed), "at {now}");
            if let Some(e) = d.front() {
                dir = dir.observe(&e, now).0;
            }
            assert_eq!(dir.clone().expire(now).1, 0, "kept at {now}");
        }
    }
}
