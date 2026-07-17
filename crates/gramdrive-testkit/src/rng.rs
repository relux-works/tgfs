//! Deterministic pseudo-randomness for scripted variation.
//!
//! Reproducibility is the whole point of this crate, so both algorithms
//! here are written out rather than pulled from a dependency: `rand`'s
//! generators carry no cross-version output stability guarantee, and
//! `DefaultHasher` explicitly reserves the right to change its output
//! between releases. A fake whose chunk boundaries shift when a dependency
//! is bumped is not a fixture — it is a flake with a seed field.
//!
//! Both are value-stable by construction: the constants are frozen here,
//! and `rng::tests` pins concrete outputs so a change to either function
//! fails the suite instead of silently re-cutting every scripted delivery.

/// SplitMix64 — Steele/Lea/Flood's finalizer-based generator.
///
/// Chosen for being a handful of frozen constants with no state beyond a
/// `u64`: the sequence is a pure function of the seed, on every platform
/// and every compiler version.
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Seeds the generator. Every seed is valid, including zero.
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next value in the sequence.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `1..=max`, or `0` when `max` is `0`.
    ///
    /// Plain modulo: the bias against a power-of-two-plus-one bound is
    /// irrelevant for picking chunk sizes, and rejection sampling would
    /// make the draw count depend on the bound — a subtler reproducibility
    /// hazard than the bias it removes.
    pub(crate) fn next_in_range(&mut self, max: u64) -> u64 {
        if max == 0 {
            return 0;
        }
        (self.next_u64() % max) + 1
    }
}

/// FNV-1a over `bytes`, used to fold an item identity into a seed.
///
/// Not a hash for security or collision resistance — a collision here only
/// means two fetches share a chunk pattern. It is here for output stability
/// alone.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the generator's output. These values are the contract: a diff
    /// here means every seeded script in the workspace re-cuts its chunks.
    #[test]
    fn splitmix64_output_is_pinned() {
        let mut rng = SplitMix64::new(0);
        assert_eq!(rng.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(rng.next_u64(), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(rng.next_u64(), 0x06c4_5d18_8009_454f);
    }

    #[test]
    fn same_seed_replays_the_same_sequence() {
        let draw = |seed| {
            let mut rng = SplitMix64::new(seed);
            (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw(42), draw(42));
        assert_ne!(draw(42), draw(43));
    }

    #[test]
    fn range_draws_stay_within_one_and_max() {
        let mut rng = SplitMix64::new(7);
        for _ in 0..512 {
            let value = rng.next_in_range(16);
            assert!((1..=16).contains(&value), "drew {value} outside 1..=16");
        }
        assert_eq!(rng.next_in_range(1), 1, "a bound of 1 can only draw 1");
        assert_eq!(rng.next_in_range(0), 0, "a bound of 0 draws nothing");
    }

    /// Pins the hash too — it feeds the per-fetch seed, so it is just as
    /// load-bearing for reproducibility as the generator.
    #[test]
    fn fnv1a_output_is_pinned() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"gramdrive"), 0x4540_7aa7_cf96_e892);
    }

    #[test]
    fn fnv1a_separates_distinct_inputs() {
        assert_ne!(fnv1a(b"item-1"), fnv1a(b"item-2"));
    }
}
