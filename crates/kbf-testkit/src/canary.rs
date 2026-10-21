//! Unpredictable bytes for the black-box probes: a [`canary`] that no file legitimately
//! holds by accident, and [`noise`] for binary inputs. Deterministic for a seed, so a
//! failing run can be repeated.

/// `n` hex characters, unpredictable to the product and different for each `seed`: an
/// incompressible canary that no file legitimately holds by accident.
#[must_use]
pub fn canary(seed: u64, n: usize) -> String {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xd1b5_4a32_d192_ed03;
    let mut out = String::with_capacity(n);
    while out.len() < n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push_str(&format!("{state:016x}"));
    }
    out.truncate(n);
    out
}

/// Binary input bytes, different for each `seed`.
#[must_use]
pub fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}
