//! Milliseconds and waiting, from the caller's runtime (always built,
//! `no_std`).
//!
//! [`Clock`] is the one time seam of the crate: the board Zenoh client
//! (`zenoh_lite`, which re-exports it as `zenoh_lite::Clock`) and the mDNS
//! announcer (`mdns::announce::MdnsAnnouncer`) take their time from it, so
//! a tokio host, an embassy board and a test's virtual clock drive the same
//! code.

/// Milliseconds and waiting, from the caller's runtime.
pub trait Clock {
    /// Milliseconds since an arbitrary, fixed start; never decreases.
    fn now_ms(&mut self) -> u64;
    /// Completes at or after `deadline_ms`. Must be cancel safe.
    async fn sleep_until_ms(&mut self, deadline_ms: u64);
}

#[cfg(all(test, feature = "zenoh-lite"))]
mod tests {
    /// A clock written against the `zenoh_lite` path only.
    struct LiteOnly(u64);

    impl crate::zenoh_lite::Clock for LiteOnly {
        fn now_ms(&mut self) -> u64 {
            self.0
        }
        async fn sleep_until_ms(&mut self, deadline_ms: u64) {
            self.0 = self.0.max(deadline_ms);
        }
    }

    fn needs_clock<C: super::Clock>(mut c: C) -> u64 {
        c.now_ms()
    }

    #[test]
    fn zenoh_lite_clock_is_the_shared_trait() {
        assert_eq!(needs_clock(LiteOnly(7)), 7);
    }
}
