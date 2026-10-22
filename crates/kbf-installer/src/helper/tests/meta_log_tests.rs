//! What the helper says when `boot-check` refused an activation for want of a copy of the
//! metadata log (`meta_log.rs`): the reason reaches `status.json`.

use super::*;

/// Catches an activation refused at its first start and not explained: the node is back on the
/// older release (the swap was reverted before the new one ran) and `status.json` says
/// `refused` with the reason `boot-check` left, once; the note is spent after it is reported.
///
/// Mutants planted: the note not read by the helper; the note left in place after it is
/// reported (the next pass would report it again over a later event).
#[test]
fn an_activation_refused_for_want_of_a_log_copy_is_explained_in_status_json() {
    let mut rig = Rig::new("meta-log-refused");
    write_kbf_toml(
        &rig.layout,
        "mode = \"storage\"\n[storage]\nmeta_dir = \"/var/lib/kbf-meta\"\n",
    );
    let log = rig.root.join("var/lib/kbf-meta");
    fs::create_dir_all(&log).expect("log");
    symlink("/etc/passwd", log.join("odd")).expect("a file that cannot be copied");

    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(B)));

    // The unit's ExecStartPre: the log cannot be saved, so B is reverted before it starts.
    rig.first_start(B, T0 + 10);
    assert_eq!(rig.current(), Some(c(A)));
    rig.cycle(T0 + 20);
    let status = rig.status();
    assert_eq!(status["state"], "refused");
    assert_eq!(status["installed_commit"], A);
    let why = rig.refusal();
    assert!(
        why.starts_with(&format!("activation of {} refused", &B[..12])),
        "{why}"
    );
    assert!(why.contains("copy of the metadata log"), "{why}");
    assert!(
        crate::meta_log::refusal(&rig.layout).is_none(),
        "the note is spent"
    );
}
