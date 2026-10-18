//! The open-file limit. systemd starts a unit with a soft `RLIMIT_NOFILE` of 1024 unless
//! the unit says otherwise (the hard limit it shows is only the ceiling), and one storage
//! node holds a file per open write-tier block, a socket per peer, per client and per
//! cold-store request, and the daemon's pipes: a build's burst of uploads passes 1024 in
//! a second and every further `open` or `connect` fails with EMFILE.

use std::io;

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The most the soft limit is raised to, whatever the hard limit allows.
pub const CEILING: u64 = 1 << 20;

/// Raises the soft open-file limit to the hard limit, at most [`CEILING`], and returns
/// the soft limit now in force. A soft limit already at or above that stays.
///
/// # Errors
///
/// The error `setrlimit` returned.
pub fn raise() -> io::Result<u64> {
    let Rlimit { current, maximum } = getrlimit(Resource::Nofile);
    let want = maximum.map_or(CEILING, |m| m.min(CEILING));
    if current.is_none_or(|c| c >= want) {
        return Ok(current.unwrap_or(want));
    }
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(want),
            maximum,
        },
    )?;
    Ok(want)
}
