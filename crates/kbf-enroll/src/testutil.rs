//! Helpers the unit tests share.

use kbf_testkit::dir::TestDir;
use kbf_types::{CaPin, Token, TokenId, TokenSecret};

/// A fresh scratch directory under the system temp dir, resolved through symlinks, removed
/// when it drops (declare it before whatever writes into it).
pub fn scratch(name: &str) -> TestDir {
    let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
    TestDir::under(temp, &format!("kbf-enroll-{name}"))
}

/// A token with distinguishable parts.
pub fn token() -> Token {
    Token::new(
        TokenId::from_bits(0x0123_4567_89ab_cdef),
        TokenSecret::from_bytes([7; 32]),
        CaPin::from_bytes([9; 32]),
    )
}
