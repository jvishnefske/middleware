//! The shared in-memory bus between two simulated payloads (`--peer`,
//! requirement R10): it decides, per message, whether delivery succeeds.
//!
//! Each message is lost with probability `loss / 1000`, drawn from a
//! SplitMix64 generator seeded by `--seed`, so a lossy run is
//! reproducible. The bus counts delivered and lost messages for the
//! summary line printed at the end of a run.

/// Per-mille loss values are `0..=MAX_LOSS`.
pub const MAX_LOSS: u16 = 1000;

/// Loss decisions and counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bus {
    loss: u16,
    state: u64,
    /// Messages handed to the peer.
    pub delivered: u64,
    /// Messages dropped by the loss model.
    pub lost: u64,
}

impl Bus {
    /// A bus losing `loss` messages per thousand (clamped to
    /// [`MAX_LOSS`]), seeded with `seed`.
    pub fn new(loss: u16, seed: u64) -> Self {
        Bus {
            loss: loss.min(MAX_LOSS),
            state: seed,
            delivered: 0,
            lost: 0,
        }
    }

    /// SplitMix64: the next pseudo-random value.
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Decides the fate of one message: `true` if it is delivered.
    pub fn pass(&mut self) -> bool {
        let lost = self.loss > 0 && self.next() % u64::from(MAX_LOSS) < u64::from(self.loss);
        if lost {
            self.lost += 1;
        } else {
            self.delivered += 1;
        }
        !lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_and_total_loss() {
        let mut bus = Bus::new(0, 1);
        assert!((0..100).all(|_| bus.pass()));
        let mut bus = Bus::new(5000, 1);
        assert!((0..100).all(|_| !bus.pass()));
        assert_eq!((bus.delivered, bus.lost), (0, 100));
    }

    #[test]
    fn loss_rate_is_roughly_honoured_and_reproducible() {
        let run = |seed| {
            let mut bus = Bus::new(250, seed);
            (0..4000).map(|_| bus.pass()).collect::<Vec<_>>()
        };
        let a = run(7);
        assert_eq!(a, run(7), "same seed, same losses");
        assert_ne!(a, run(8));
        let lost = a.iter().filter(|d| !**d).count();
        assert!((800..1200).contains(&lost), "{lost}");
    }
}
