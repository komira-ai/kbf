//! The daemon process's claim on its scratch directory (issue #22): the file
//! `<--scratch>/daemon-instance` holds the instance id of the daemon process that runs
//! there, so the next one can name it in its `Hello` as its predecessor.
//!
//! The claim is what lets the server give up the predecessor's leases at once: the
//! drivers' start-up sweep already assumes one daemon per scratch directory (a second
//! would kill the first's runs), and a daemon claims the directory only once every
//! runtime has been built, so once every sweep has killed what the predecessor left.
//! A daemon that fails to start leaves the claim it found. One whose claim cannot be
//! read or written starts all the same, naming no predecessor: the server then keeps
//! the predecessor's leases for the handover grace, as without a claim.

use std::io::{self, Write as _};
use std::path::Path;

/// The claim's file name in the scratch directory.
pub(crate) const FILE: &str = "daemon-instance";

/// Claims `scratch` for the daemon process `own`: returns the instance id the claim
/// held (empty when there was none), then writes `own` in its place, whole, through a
/// temporary file renamed over it. Makes `scratch` if it is missing.
///
/// # Errors
/// The claim cannot be read (other than being absent) or written.
pub(crate) fn claim(scratch: &Path, own: &str) -> io::Result<String> {
    std::fs::create_dir_all(scratch)?;
    let path = scratch.join(FILE);
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => text.trim().to_owned(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let temporary = scratch.join(format!("{FILE}.new"));
    let mut file = std::fs::File::create(&temporary)?;
    writeln!(file, "{own}")?;
    file.sync_all()?;
    std::fs::rename(&temporary, &path)?;
    Ok(before)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::scratch;

    /// Catches a first start that fails for want of a claim or of the directory, a
    /// claim read with its newline (the server would not match it), and a claim not
    /// replaced (the next daemon would name a process two starts back).
    #[test]
    fn each_claim_returns_the_one_before() {
        let dir = scratch("claim").join("leases");
        assert_eq!(claim(&dir, "one").expect("claimed"), "");
        assert_eq!(claim(&dir, "two").expect("claimed"), "one");
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE)).expect("read"),
            "two\n"
        );
        assert!(!dir.join("daemon-instance.new").exists());
    }

    /// Catches a claim that cannot be read taken as an empty one, and one that cannot be
    /// written taken as written: the caller is told either way (and names no
    /// predecessor).
    #[test]
    fn a_claim_that_cannot_be_read_or_written_is_an_error() {
        let dir = scratch("claim-unreadable");
        std::fs::create_dir_all(dir.join(FILE)).expect("a directory where the claim goes");
        assert!(claim(&dir, "one").is_err());
        let other = scratch("claim-unwritable");
        std::fs::create_dir_all(other.join("daemon-instance.new")).expect("in the way");
        assert!(claim(&other, "one").is_err());
    }
}
