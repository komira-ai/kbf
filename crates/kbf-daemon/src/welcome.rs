//! Which `Welcome` versions a daemon accepts.
//!
//! A server answers a `Hello` of a version it speaks with that version, and a `Hello` of
//! a newer version than it speaks with its own newest. A daemon therefore accepts a
//! `Welcome` below the version its `Hello` named, down to the oldest it speaks, and never
//! one above: it would be told a version it cannot be sure the server speaks.

/// The oldest `kbf.worker.v1` protocol version this daemon speaks.
pub(crate) const OLDEST_PROTOCOL_VERSION: u32 = 1;

/// Whether a `Welcome` naming `welcome` answers a `Hello` that named `hello_sent`, for a
/// daemon speaking `oldest` to `newest`.
pub(crate) fn acceptable(welcome: u32, oldest: u32, hello_sent: u32, newest: u32) -> bool {
    (oldest..=hello_sent.min(newest)).contains(&welcome)
}

#[cfg(test)]
mod tests {
    use super::acceptable;

    /// Catches: a daemon that takes only its own version (a Welcome below it, from a server
    /// older than the daemon, ends every session of an updated daemon), one that takes a
    /// version above its Hello's (the server would speak a version the daemon did not
    /// offer), and one that takes a version below the oldest it speaks.
    #[test]
    fn a_welcome_is_accepted_from_the_oldest_up_to_the_hello_sent() {
        for welcome in 1..=3 {
            assert!(acceptable(welcome, 1, 3, 3), "welcome {welcome}");
        }
        for welcome in [0, 4, u32::MAX] {
            assert!(!acceptable(welcome, 1, 3, 3), "welcome {welcome}");
        }
        // A Hello that named less than the daemon's newest (a daemon that offers an older
        // version) accepts nothing above what it named.
        assert!(acceptable(2, 1, 2, 3));
        assert!(!acceptable(3, 1, 2, 3));
        // Where the daemon speaks one version, it is the only one accepted.
        assert!(acceptable(1, 1, 1, 1));
        assert!(!acceptable(2, 1, 1, 1));
        assert!(!acceptable(0, 1, 1, 1));
    }
}
