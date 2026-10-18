use super::*;
use crate::testkit::{B, c};
use kbf_host::testkit::FakeExec;

fn pending(phase: Phase) -> FrontendPending {
    FrontendPending {
        epoch: 2,
        order_id: 9,
        commit: c(B),
        idle: Instance::B,
        old: Instance::A,
        prior_slot: Some(format!("../releases/{}", "a".repeat(40))),
        deadline_s: 1_800_000_600,
        phase,
        why: String::new(),
    }
}

/// Pins the record's spelling, which the next state schema takes over as two fields
/// (`serving_instance`: an instance as `"a"` or `"b"`, and `frontend_pending`): the field
/// names, the lower-case instances, the phase as a tagged word, and `why` only once there is
/// one. A rename would strand a pending update across an installer upgrade.
#[test]
fn the_record_is_spelled_as_the_state_schema_will_spell_it() {
    let file = FrontendFile {
        serving_instance: Some(Instance::A),
        frontend_pending: Some(pending(Phase::Confirming { looks: 2 })),
        last: None,
    };
    let json = serde_json::to_value(&file).expect("json");
    assert_eq!(
        json,
        serde_json::json!({
            "serving_instance": "a",
            "frontend_pending": {
                "epoch": 2,
                "order_id": 9,
                "commit": B,
                "idle": "b",
                "old": "a",
                "prior_slot": format!("../releases/{}", "a".repeat(40)),
                "deadline_s": 1_800_000_600u64,
                "phase": { "phase": "confirming", "looks": 2 }
            }
        })
    );
    for phase in [
        Phase::Confirming { looks: 0 },
        Phase::Flipping,
        Phase::Reverting {
            deadline: 7,
            old_started: true,
        },
        Phase::NeedsOperator {
            reason: "x".to_owned(),
        },
    ] {
        let mut p = pending(phase);
        p.why = "the new instance stopped".to_owned();
        let file = FrontendFile {
            serving_instance: None,
            frontend_pending: Some(p),
            last: None,
        };
        let back: FrontendFile =
            serde_json::from_slice(&serde_json::to_vec(&file).expect("bytes")).expect("back");
        assert_eq!(back, file);
    }
    assert_eq!(
        serde_json::to_string(&FrontendFile::default()).expect("json"),
        "{}"
    );
}

/// Catches an unknown key being ignored (a record from a newer installer read as if whole)
/// and a missing file being an error rather than an empty record; `save` and `load` agree,
/// and the file is root's alone.
#[test]
fn a_missing_file_is_empty_an_unknown_key_is_refused_and_save_then_load_agree() {
    let (layout, root) = crate::testkit::host_root("frontend-file");
    assert_eq!(load(&layout).expect("none"), FrontendFile::default());
    std::fs::write(
        root.join("var/lib/kbf-deploy/frontend.json"),
        br#"{"surprise":1}"#,
    )
    .expect("write");
    assert!(matches!(load(&layout), Err(FileError::Parse(_))));
    std::fs::write(root.join("var/lib/kbf-deploy/frontend.json"), b"{").expect("write");
    assert!(matches!(load(&layout), Err(FileError::Parse(_))));
    let mut exec = FakeExec::new(0);
    let file = FrontendFile {
        serving_instance: Some(Instance::B),
        frontend_pending: None,
        last: None,
    };
    save(&mut exec, &layout, &file).expect("save");
    let Step::WriteFile {
        path, perms, bytes, ..
    } = &exec.log[0]
    else {
        panic!("{:?}", exec.log)
    };
    assert_eq!(path.as_str(), "/var/lib/kbf-deploy/frontend.json");
    assert_eq!(*perms, Perms::new(0o600));
    assert_eq!(
        serde_json::from_slice::<FrontendFile>(bytes).expect("json"),
        file
    );
    std::fs::write(root.join("var/lib/kbf-deploy/frontend.json"), bytes).expect("write");
    assert_eq!(load(&layout).expect("loads"), file);
    std::fs::write(root.join("var/lib/kbf-deploy/frontend.json"), b"").expect("write");
    assert!(load(&layout).is_err());
    std::fs::remove_file(root.join("var/lib/kbf-deploy/frontend.json")).expect("rm");
    std::fs::create_dir(root.join("var/lib/kbf-deploy/frontend.json")).expect("dir");
    assert!(matches!(load(&layout), Err(FileError::Read(_))));
}
