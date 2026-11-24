//! The one random source of the generator: SplitMix64, seeded.
//!
//! Why our own, and why this one: a scenario must be the same for a seed
//! on every machine and every compiler version, for ever (I13, and the
//! seed archive of docs/sync.md 7.2). An outside crate's generator may
//! change its stream between versions; SplitMix64 is ten lines whose
//! output is fixed by its published constants, and it is good enough for
//! choosing test operations (it is not used for anything secret).

/// A seeded, deterministic random number generator (SplitMix64).
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// A generator whose whole stream is fixed by `seed`.
    pub fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        // SplitMix64 (Steele, Lea, Flood 2014), constants as published.
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`; `n` of zero gives zero rather than a panic, so
    /// a caller choosing from an empty list gets "nothing to choose"
    /// through its own `get` instead of a crash.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        // The modulo bias is below 2^-40 for the small n used here.
        (self.next_u64() % n as u64) as usize
    }

    /// True with probability `percent` / 100.
    pub fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent as usize
    }

    /// One element of `items`, or `None` when it is empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            items.get(self.below(items.len()))
        }
    }

    /// An index chosen with the given weights (a weight of zero is never
    /// chosen). `None` when every weight is zero.
    pub fn weighted(&mut self, weights: &[u32]) -> Option<usize> {
        let total: u64 = weights.iter().map(|w| u64::from(*w)).sum();
        if total == 0 {
            return None;
        }
        let mut roll = self.next_u64() % total;
        for (i, w) in weights.iter().enumerate() {
            let w = u64::from(*w);
            if roll < w {
                return Some(i);
            }
            roll -= w;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    /// The stream for a seed never changes: these are SplitMix64's
    /// published first outputs for seed 0... and the archive depends on it.
    #[test]
    fn stream_is_fixed() {
        let mut r = Rng::new(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    }

    #[test]
    fn weighted_skips_zero() {
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            assert_eq!(r.weighted(&[0, 3, 0]), Some(1));
        }
        assert_eq!(r.weighted(&[0, 0]), None);
    }
}
