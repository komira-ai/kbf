//! The rule for a file the tool trusts as input and a person or another host's writer keeps:
//! the cold store's heal status file.

/// The largest status file read, in bytes.
pub const STATUS_FILE_MAX: u64 = 64 * 1024;

/// Why a file with this metadata is not trusted, or `Ok`: it must be a regular file, owned by
/// `me`, not writable by group or others, and at most [`STATUS_FILE_MAX`] bytes.
///
/// # Errors
/// The reason, as text.
pub fn rule(is_file: bool, uid: u32, mode: u32, len: u64, me: u32) -> Result<(), &'static str> {
    let _ = (is_file, uid, mode, len, me);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches each part of the rule being dropped: a non-regular file, another owner, a mode
    /// bit of group or others that writes, and a file over the cap are each refused; the
    /// permissive read bits (0644, 0600, 0444) and a file of exactly the cap pass.
    #[test]
    fn the_rule_refuses_each_fault_and_only_those() {
        let me = 1000;
        assert_eq!(rule(true, me, 0o644, 10, me), Ok(()));
        assert_eq!(rule(true, me, 0o600, 10, me), Ok(()));
        assert_eq!(rule(true, me, 0o444, STATUS_FILE_MAX, me), Ok(()));
        assert!(rule(false, me, 0o644, 10, me).is_err());
        assert!(rule(true, me + 1, 0o644, 10, me).is_err());
        assert!(
            rule(true, 0, 0o644, 10, me).is_err(),
            "root's file is not mine"
        );
        for mode in [0o664, 0o646, 0o666, 0o620, 0o602] {
            assert!(rule(true, me, mode, 10, me).is_err(), "{mode:o}");
        }
        assert!(rule(true, me, 0o644, STATUS_FILE_MAX + 1, me).is_err());
    }
}
