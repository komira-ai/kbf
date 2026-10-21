//! Certificate serial numbers.

use crate::entropy::Entropy;

/// A serial: 127 random bits, never zero (a draw of zero is drawn again).
pub(crate) fn draw(entropy: &mut dyn Entropy) -> u128 {
    loop {
        let mut block = [0u8; 16];
        entropy.fill(&mut block);
        block[0] &= 0x7f;
        let serial = u128::from_be_bytes(block);
        if serial != 0 {
            return serial;
        }
    }
}
