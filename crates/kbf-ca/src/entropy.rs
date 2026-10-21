//! The random source, as a parameter.

use ring::rand::{SecureRandom, SystemRandom};

/// A source of random bytes. Serials are drawn from one; a test supplies a scripted one.
pub trait Entropy {
    /// Fills `dest` with random bytes.
    fn fill(&mut self, dest: &mut [u8]);
}

/// The operating system's random source. It cannot fail in a way a caller could repair,
/// and a CA that signed with a failed random source would issue guessable serials, so a
/// failure stops the process.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEntropy;

impl Entropy for SystemEntropy {
    fn fill(&mut self, dest: &mut [u8]) {
        SystemRandom::new()
            .fill(dest)
            .expect("the operating system's random source failed");
    }
}
