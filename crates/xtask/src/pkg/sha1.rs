//! SHA-1 and the POSIX `cksum` CRC, the two checksums the installer package format
//! asks for.
//!
//! A xar archive names its table of contents and each member by SHA-1, and a bill of
//! materials names each file by its `cksum` CRC. Neither guards anything: the package's
//! signature, added later by `productsign`, does. They are written here, a screen each
//! and tested against the standard vectors, so the workspace gains no crate for them.

/// The SHA-1 digest of `data` (FIPS 180-4).
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    // Zeros up to 56 bytes into a 64-byte block, then the bit length.
    msg.resize(msg.len() + (119 - data.len() % 64) % 64, 0);
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (slot, word) in w.iter_mut().zip(block.chunks_exact(4)) {
            *slot = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (chunk, word) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// The lowercase hex of [`sha1`]`(data)`.
pub fn sha1_hex(data: &[u8]) -> String {
    hex::encode(sha1(data))
}

/// The CRC `cksum(1)` prints for `data` (POSIX: polynomial 0x04C11DB7, most significant
/// bit first, then the length, least significant byte first, then complemented).
pub fn cksum(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    let mut feed = |byte: u8| {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ (0x04C1_1DB7 * (crc >> 31));
        }
    };
    data.iter().for_each(|&b| feed(b));
    let mut len = data.len();
    while len != 0 {
        feed((len & 0xFF) as u8);
        len >>= 8;
    }
    !crc
}
