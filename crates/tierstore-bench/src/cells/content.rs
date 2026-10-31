//! Seeded bytes: the same seed gives the same bytes on every run, so a body or a tree is not
//! a thing to store, and a rerun writes what the first run wrote.
//!
//! The generator is SplitMix64, which is enough for content that only has to be not compressible
//! and not constant; it is not a source of secrets.

use bytes::Bytes;

/// SplitMix64: one 64-bit state, a fixed sequence for a seed.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator for `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..bound`; `bound` must not be zero. The bias of the modulo is of no
    /// consequence for a bootstrap resample or a size draw.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// A number in `[0, 1)` with 53 bits.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// `len` bytes from `seed`, the same for the same pair.
#[must_use]
pub fn fill(seed: u64, len: u64) -> Vec<u8> {
    let len = usize::try_from(len).expect("a body fits in memory");
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(len);
    out
}

/// The body of `len` bytes for `seed`, as the cheap-to-clone [`Bytes`] a put takes.
#[must_use]
pub fn body(seed: u64, len: u64) -> Bytes {
    Bytes::from(fill(seed, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a generator whose output depends on anything but the seed (a rerun would write
    /// other bytes), or two seeds giving one stream.
    #[test]
    fn the_same_seed_gives_the_same_bytes_and_another_seed_other_bytes() {
        assert_eq!(fill(7, 100), fill(7, 100));
        assert_ne!(fill(7, 100), fill(8, 100));
        assert_eq!(body(7, 100).as_ref(), fill(7, 100).as_slice());
    }

    /// Catches a length that is rounded to the generator's 8-byte step, at, below and above a
    /// step, and the empty body.
    #[test]
    fn the_length_is_exact() {
        for len in [0, 1, 7, 8, 9, 4096, 19 * 1024] {
            assert_eq!(fill(1, len).len() as u64, len);
        }
    }

    /// Catches a prefix that changes with the length: a shorter body must be the start of a
    /// longer one of the same seed.
    #[test]
    fn a_shorter_body_is_the_start_of_a_longer_one() {
        assert_eq!(fill(3, 13), fill(3, 64)[..13]);
    }

    /// Catches `below` leaving its range and `unit` leaving `[0, 1)`.
    #[test]
    fn draws_stay_in_range() {
        let mut rng = Rng::new(9);
        for _ in 0..1000 {
            assert!(rng.below(10) < 10);
            let u = rng.unit();
            assert!((0.0..1.0).contains(&u));
        }
    }
}
