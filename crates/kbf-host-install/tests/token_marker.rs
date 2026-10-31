//! The built helper binary carries the mark that its channel token file takes GitHub's Actions
//! token (`LONG_TOKEN_MARKER`). The update rehearsal reads that mark from the release before it
//! to decide whether to give the helper the run's token; a binary whose linker or profile
//! dropped the mark would leave every rehearsal anonymous without a failure.
//!
//! Catches: the mark being dropped from the binary (dead-code removal, a changed profile), and
//! the mark the rehearsal looks for drifting from the one the helper carries.

const MARKER: &[u8] = b"kbf-host-install:token-file-takes-actions-tokens-WRONG";

#[test]
fn the_helper_binary_carries_the_mark_the_rehearsal_looks_for() {
    let path = env!("CARGO_BIN_EXE_kbf-host-install");
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(
        bytes.windows(MARKER.len()).any(|w| w == MARKER),
        "{path} does not carry the mark"
    );
}
