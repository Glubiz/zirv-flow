//! The simulation's only source of randomness: SplitMix64, seeded, with no platform input, so a
//! seed names one run on every machine.

/// A SplitMix64 generator.
#[derive(Clone, Debug)]
pub struct Rng(u64);

/// One SplitMix64 output step over `value`: a stateless hash of a `u64`.
pub const fn mix(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(mix(seed))
    }

    /// A generator for one named stream of `seed`: the same parts always give the same stream,
    /// whatever else the run did first.
    pub fn derive(seed: u64, parts: &[u64]) -> Self {
        let state = parts
            .iter()
            .fold(mix(seed), |state, part| mix(state ^ mix(*part)));
        Self(state)
    }

    pub fn next_u64(&mut self) -> u64 {
        let out = mix(self.0);
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        out
    }

    /// A value in `0..bound` (`bound` of zero gives zero).
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }

    /// A value in `low..=high`.
    pub fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high.saturating_sub(low).saturating_add(1))
    }

    /// An index into a list of `len` items.
    pub fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.below(len as u64)).unwrap_or(0)
    }

    /// True `percent` times in a hundred.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.index(items.len())]
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for end in (1..items.len()).rev() {
            let other = self.index(end + 1);
            items.swap(end, other);
        }
    }

    pub fn uuid(&mut self) -> uuid::Uuid {
        let high = u128::from(self.next_u64());
        uuid::Uuid::from_u128((high << 64) | u128::from(self.next_u64()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_names_one_stream() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let first: Vec<u64> = (0..5).map(|_| a.next_u64()).collect();
        let second: Vec<u64> = (0..5).map(|_| b.next_u64()).collect();
        assert_eq!(first, second);
        let other: Vec<u64> = {
            let mut c = Rng::new(8);
            (0..5).map(|_| c.next_u64()).collect()
        };
        assert_ne!(first, other);
    }

    #[test]
    fn the_first_outputs_are_pinned() {
        // Pinned so a change to the generator cannot silently change every fixed seed's run.
        let mut rng = Rng::new(0);
        assert_eq!(rng.next_u64(), mix(mix(0)));
        assert_eq!(mix(0), 0xe220_a839_7b1d_cdaf);
    }

    #[test]
    fn derived_streams_depend_only_on_their_parts() {
        let a = Rng::derive(1, &[2, 3]).next_u64();
        assert_eq!(a, Rng::derive(1, &[2, 3]).next_u64());
        assert_ne!(a, Rng::derive(1, &[3, 2]).next_u64());
        assert_ne!(a, Rng::derive(2, &[2, 3]).next_u64());
    }

    #[test]
    fn bounds_hold() {
        let mut rng = Rng::new(1);
        for _ in 0..1000 {
            assert!(rng.below(7) < 7);
            assert!((3..=5).contains(&rng.range(3, 5)));
            assert!(rng.index(4) < 4);
        }
        assert_eq!(rng.below(0), 0);
        let mut items: Vec<u32> = (0..20).collect();
        rng.shuffle(&mut items);
        items.sort_unstable();
        assert_eq!(items, (0..20).collect::<Vec<u32>>());
    }
}
