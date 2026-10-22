//! The gates, against scripted helpers: what each verdict is made of.

use serde_json::json;

use crate::cli::Gate;
use crate::fake::{FakeSystem, Script, ctx, good_signature, tmp, work_with_base};
use crate::gates::{self, g1, g2, g3, g5, p0, p2, p6};
use crate::guest::{ROOT_RESULT, USER_RESULT};
use crate::report::Status;
use crate::spec::{Event, LIMIT_EXCEEDED};
use crate::system::{Poll, Signature};

fn user_result(exit: i64) -> String {
    json!({"mount_custom_tag": {"attempted": true, "exit_code": exit}, "automount": {"dir_readable": true}}).to_string()
}

/// Catches a gate that runs without a base, or says error where a person must act:
/// every gate reports needs-manual and names the missing base.
#[test]
fn every_gate_without_a_base_needs_a_person() {
    let sys = FakeSystem::new();
    let work = tmp("gates-nobase");
    let c = ctx(&sys, &work);
    for gate in [
        Gate::G1,
        Gate::G2,
        Gate::G3,
        Gate::G5,
        Gate::P0,
        Gate::P2,
        Gate::P6,
    ] {
        let r = gates::run(&c, gate, None);
        assert_eq!(r.status, Status::NeedsManual, "{gate:?}");
        assert!(
            r.reason.expect("reason").contains("prepared base"),
            "{gate:?}"
        );
        assert_eq!(r.probe, format!("{gate:?}").to_lowercase());
    }
    assert!(sys.starts.borrow().is_empty());
}

/// Catches G1 arms that are run with the wrong identity (the fresh arm cloning the base
/// identifier, the same-identifier arm regenerating it) or two clones that are not both
/// started before either is judged.
#[test]
fn g1_runs_two_clones_per_identity_and_reports_each() {
    let work = work_with_base("g1-ok");
    let sys = FakeSystem::new();
    let c = ctx(&sys, &work);
    let r = g1(&c);
    assert_eq!(r.status, Status::Ok);
    assert_eq!(r.details["same_identifier"]["booted"], 2);
    assert_eq!(r.details["new_identifier"]["booted"], 2);
    assert!(
        r.summary
            .expect("summary")
            .contains("same identifier: 2 of 2 booted at once")
    );
    // 2 same-identifier clones of four files, 2 new-identifier clones of three.
    assert_eq!(sys.cloned.borrow().len(), 8 + 6);
    assert_eq!(sys.starts.borrow().len(), 4);
    assert!(!work.join("g1-same-0").exists(), "clones are removed");
}

/// Catches a failed boot being counted as one, and the failure text being dropped.
#[test]
fn g1_reports_a_clone_that_does_not_boot() {
    let work = work_with_base("g1-one-fails");
    let sys = FakeSystem::new();
    sys.script(Script::booted())
        .script(Script::failing("machine identifier already in use", None));
    let r = g1(&ctx(&sys, &work));
    assert_eq!(r.details["same_identifier"]["booted"], 1);
    assert_eq!(
        r.details["same_identifier"]["vms"][1]["failure"],
        "machine identifier already in use"
    );
}

/// Catches clone and identifier failures reported as an answer about the framework.
#[test]
fn g1_infrastructure_failures_are_errors() {
    let work = work_with_base("g1-err");
    let sys = FakeSystem::new();
    *sys.clone_fails_on.borrow_mut() = Some("disk.img");
    assert_eq!(g1(&ctx(&sys, &work)).status, Status::Error);
    *sys.clone_fails_on.borrow_mut() = None;
    *sys.identity.borrow_mut() = Err("no".into());
    let r = g1(&ctx(&sys, &work));
    assert_eq!(r.status, Status::Error);
    assert!(r.reason.expect("reason").contains("no"));
    // The first clone works, the second fails: the first is stopped and removed.
    *sys.identity.borrow_mut() = Ok(vec![1]);
    sys.clone_budget.set(Some(4));
    let r = g1(&ctx(&sys, &work));
    assert_eq!(r.status, Status::Error);
    assert!(!work.join("g1-same-0").exists());
}

/// Catches a channel verdict that does not follow what the host saw: vsock wins over
/// console, console over nothing, and the guest's own file is kept in the details.
#[test]
fn g2_names_the_channel_the_host_saw() {
    for (traffic, want) in [
        (vec![Poll::Event(Event::Vsock { port: 5000 })], "vsock"),
        (
            vec![Poll::Event(Event::Console {
                port: "kbf.probe".into(),
                text: "x".into(),
            })],
            "console",
        ),
        (vec![], "none"),
        (
            vec![
                Poll::Event(Event::Vsock { port: 5000 }),
                Poll::Event(Event::Console {
                    port: "kbf.probe".into(),
                    text: "x".into(),
                }),
            ],
            "vsock",
        ),
    ] {
        let work = work_with_base("g2-channel");
        let sys = FakeSystem::new();
        let mut s = Script::booted().with_guest_file(
            USER_RESULT,
            &json!({"vsock": {"socket_errno": 47}}).to_string(),
        );
        for p in traffic {
            s = s.then(p);
        }
        sys.script(s);
        let r = g2(&ctx(&sys, &work));
        assert_eq!(r.status, Status::Ok, "{want}: {r:?}");
        assert_eq!(r.details["channel"], want);
        assert_eq!(r.details["guest"][USER_RESULT]["vsock"]["socket_errno"], 47);
    }
}

/// Catches a probe that reports an answer when the guest ran nothing of ours: it is
/// needs-manual, and the reason says whether the binary was given.
#[test]
fn g2_without_a_guest_agent_needs_a_person() {
    let work = work_with_base("g2-noagent");
    let sys = FakeSystem::new();
    let r = g2(&ctx(&sys, &work));
    assert_eq!(r.status, Status::NeedsManual);
    assert!(r.reason.expect("reason").contains("--guest-binary"));
    let bin = work.join("guest-bin");
    std::fs::write(&bin, "x").expect("bin");
    let mut c = ctx(&sys, &work);
    c.guest_binary = Some(bin);
    let r = g2(&c);
    assert!(!r.reason.expect("reason").contains("--guest-binary"));
    assert!(r.details["boot"]["booted"].as_bool().expect("booted"));
}

/// Catches a probe that goes on with a guest that did not boot, or a boot error read as
/// an answer.
#[test]
fn g2_and_g3_report_a_guest_that_does_not_boot_as_an_error() {
    let work = work_with_base("g23-noboot");
    let sys = FakeSystem::new();
    sys.script(Script::failing("no disk", None));
    let r = g2(&ctx(&sys, &work));
    assert_eq!(r.status, Status::Error);
    assert!(r.reason.expect("reason").contains("did not boot: no disk"));
    *sys.start_error.borrow_mut() = Some("cannot spawn".into());
    let r = g3(&ctx(&sys, &work));
    assert_eq!(r.status, Status::Error);
    assert!(r.reason.expect("reason").contains("cannot spawn"));
}

/// Catches a needs-root verdict that ignores the root control: the verdict is true only
/// when the unprivileged mount failed and the root one worked.
#[test]
fn g3_compares_the_user_with_the_root_control() {
    for (user, root, want) in [
        (1, 0, json!(true)),
        (0, 0, json!(false)),
        (1, 1, json!(false)),
        (0, 1, json!(false)),
    ] {
        let work = work_with_base("g3-verdict");
        let sys = FakeSystem::new();
        sys.script(
            Script::booted()
                .with_guest_file(USER_RESULT, &user_result(user))
                .with_guest_file(ROOT_RESULT, &user_result(root)),
        );
        let r = g3(&ctx(&sys, &work));
        assert_eq!(r.status, Status::Ok);
        assert_eq!(
            r.details["custom_tag_mount_needs_root"], want,
            "user {user} root {root}"
        );
        assert_eq!(r.details["automount_readable_without_root"], true);
    }
}

/// Catches a verdict invented from a result that has no mount attempt, and one file
/// arriving without the other being treated as an answer.
#[test]
fn g3_without_an_attempt_has_no_verdict_and_without_a_file_needs_a_person() {
    let work = work_with_base("g3-partial");
    let sys = FakeSystem::new();
    sys.script(
        Script::booted()
            .with_guest_file(USER_RESULT, r#"{"mount_custom_tag": {"attempted": false}}"#)
            .with_guest_file(ROOT_RESULT, "{}"),
    );
    let r = g3(&ctx(&sys, &work));
    assert!(r.details["custom_tag_mount_needs_root"].is_null());
    assert!(r.details["automount_readable_without_root"].is_null());
    let work = work_with_base("g3-onefile");
    sys.script(Script::booted().with_guest_file(USER_RESULT, &user_result(1)));
    let r = g3(&ctx(&sys, &work));
    assert_eq!(r.status, Status::NeedsManual);
    assert!(r.reason.expect("reason").contains(ROOT_RESULT));
}

/// Catches G5 arms that do not cover graphics on and off, as the current user and as the
/// throwaway uid, a throwaway uid used without root, and conditions that are not named.
#[test]
fn g5_runs_every_arm_it_can_and_names_what_is_missing() {
    let work = work_with_base("g5-root");
    let sys = FakeSystem::new();
    sys.uid.set(0);
    sys.ppid.set(1);
    sys.console_uid.set(Some(0));
    let r = g5(&ctx(&sys, &work), Some(777));
    assert_eq!(r.status, Status::Ok, "{r:?}");
    assert_eq!(r.details["arms"].as_array().expect("arms").len(), 4);
    let starts = sys.starts.borrow();
    let ran: Vec<(bool, Option<u32>)> = starts.iter().map(|(s, u)| (s.graphics, *u)).collect();
    assert_eq!(
        ran,
        vec![
            (false, None),
            (true, None),
            (false, Some(777)),
            (true, Some(777))
        ]
    );
    drop(starts);

    let work = work_with_base("g5-user");
    let sys = FakeSystem::new();
    sys.console_uid.set(None);
    let r = g5(&ctx(&sys, &work), None);
    assert_eq!(r.status, Status::NeedsManual);
    let why = r.reason.expect("reason");
    assert!(
        why.contains("not a child of launchd")
            && why.contains("not root")
            && why.contains("logged in"),
        "{why}"
    );
    assert_eq!(r.details["arms"].as_array().expect("arms").len(), 2);
    assert_eq!(r.details["throwaway_uid"], gates::THROWAWAY_UID);
    assert!(sys.starts.borrow().iter().all(|(_, u)| u.is_none()));
}

/// Catches an arm whose clone fails being dropped from the report.
#[test]
fn g5_records_an_arm_that_cannot_start() {
    let work = work_with_base("g5-fail");
    let sys = FakeSystem::new();
    sys.ppid.set(1);
    sys.console_uid.set(Some(0));
    *sys.clone_fails_on.borrow_mut() = Some("disk.img");
    let r = g5(&ctx(&sys, &work), None);
    assert_eq!(r.details["arms"][0]["result"]["booted"], false);
    assert!(
        r.details["arms"][0]["result"]["failure"]
            .as_str()
            .expect("text")
            .contains("EXDEV")
    );
}

fn p2_scripts(sys: &FakeSystem, after_kill: Vec<Script>) {
    sys.script(Script::booted())
        .script(Script::booted())
        .script(Script::limit());
    for s in after_kill {
        sys.script(s);
    }
}

/// Catches a third VM that is not attempted while two run, a kill that is not SIGKILL of
/// the first helper, and a restart latency that is not reported.
#[test]
fn p2_reports_the_limit_error_and_the_start_after_sigkill() {
    let work = work_with_base("p2-ok");
    let sys = FakeSystem::new();
    p2_scripts(&sys, vec![Script::limit(), Script::booted()]);
    let r = p2(&ctx(&sys, &work));
    assert_eq!(r.status, Status::Ok, "{r:?}");
    assert_eq!(r.details["third_hit_limit"], true);
    assert_eq!(r.details["third"]["code"], LIMIT_EXCEEDED);
    assert_eq!(r.details["killed_helper_pid"], 1000);
    assert_eq!(r.details["start_after_sigkill"]["started"], true);
    assert_eq!(r.details["start_after_sigkill"]["attempts"], 2);
    assert_eq!(
        r.details["start_after_sigkill"]["first_attempt_succeeded"],
        false
    );
    // The restart reuses the killed VM's own bundle.
    assert_eq!(sys.starts.borrow()[3].0.bundle, work.join("p2-0"));
}

/// Catches a third VM that boots being reported as the limit, and a start that never
/// works after the kill being hidden.
#[test]
fn p2_reports_a_third_that_boots_and_a_slot_that_never_frees() {
    let work = work_with_base("p2-odd");
    let sys = FakeSystem::new();
    sys.script(Script::booted())
        .script(Script::booted())
        .script(Script::booted());
    let mut after = Vec::new();
    after.extend((0..200).map(|_| Script::limit()));
    for s in after {
        sys.script(s);
    }
    let r = p2(&ctx(&sys, &work));
    assert_eq!(r.details["third_hit_limit"], false);
    assert_eq!(r.details["start_after_sigkill"]["started"], false);
    assert!(
        r.details["start_after_sigkill"]["failure"]
            .as_str()
            .expect("text")
            .contains("still at the guest limit")
    );
}

/// Catches P2 carrying on when the first two guests cannot both start, and clone
/// failures at each of the three starts.
#[test]
fn p2_infrastructure_failures_are_errors() {
    let work = work_with_base("p2-err");
    let sys = FakeSystem::new();
    sys.script(Script::booted())
        .script(Script::failing("no", None));
    let r = p2(&ctx(&sys, &work));
    assert_eq!(r.status, Status::Error);
    assert!(r.reason.expect("reason").contains("did not both boot"));
    for budget in [0, 4, 8] {
        let sys = FakeSystem::new();
        sys.clone_budget.set(Some(budget));
        let r = p2(&ctx(&sys, &work));
        assert_eq!(r.status, Status::Error, "budget {budget}");
    }
}

/// Catches P0 attempting the write without root (the question is a root write), writing
/// to the base instead of a clone, and a failed write reported as an error rather than
/// as the answer.
#[test]
fn p0_writes_the_payload_into_a_clone_as_root() {
    let work = work_with_base("p0-ok");
    let sys = FakeSystem::new();
    let r = p0(&ctx(&sys, &work));
    assert_eq!(r.status, Status::NeedsManual);
    assert!(r.reason.expect("reason").contains("needs root"));
    assert!(sys.offline_calls.borrow().is_empty());
    sys.uid.set(0);
    let r = p0(&ctx(&sys, &work));
    assert_eq!(
        (r.status, r.details["worked"].clone()),
        (Status::Ok, json!(true))
    );
    let calls = sys.offline_calls.borrow();
    assert_eq!(calls[0].0, work.join("p0-vm").join("disk.img"));
    assert!(calls[0].1.iter().any(|p| p.ends_with(".AppleSetupDone")));
    drop(calls);
    assert!(!work.join("p0-vm").exists(), "the clone is removed");
    *sys.offline.borrow_mut() = Err("not permitted".into());
    let r = p0(&ctx(&sys, &work));
    assert_eq!(
        (r.status, r.details["worked"].clone()),
        (Status::Ok, json!(false))
    );
    assert_eq!(r.details["failure"], "not permitted");
    sys.clone_fails_on.replace(Some("disk.img"));
    assert_eq!(p0(&ctx(&sys, &work)).status, Status::Error);
}

/// Catches P6 starting a VM from a binary that does not have the signature the question
/// is about, a signature error reported as an answer, and the verdict sentence not
/// following the boot.
#[test]
fn p6_needs_the_signature_it_asks_about() {
    let work = work_with_base("p6");
    let sys = FakeSystem::new();
    let r = p6(&ctx(&sys, &work));
    assert_eq!(r.status, Status::Ok);
    assert_eq!(r.details["boot"]["booted"], true);
    assert!(r.summary.expect("summary").starts_with("a VM ran"));
    sys.script(Script::failing("killed by amfid", None));
    let r = p6(&ctx(&sys, &work));
    assert!(r.summary.expect("summary").starts_with("a VM did not run"));
    for sig in [
        Signature {
            adhoc: false,
            ..good_signature()
        },
        Signature {
            hardened_runtime: false,
            ..good_signature()
        },
        Signature {
            virtualization_entitlement: false,
            ..good_signature()
        },
    ] {
        *sys.sig.borrow_mut() = Ok(sig);
        let r = p6(&ctx(&sys, &work));
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("sign-darwin"));
    }
    *sys.sig.borrow_mut() = Err("no codesign".into());
    assert_eq!(p6(&ctx(&sys, &work)).status, Status::Error);
    *sys.sig.borrow_mut() = Ok(good_signature());
    sys.clone_fails_on.replace(Some("disk.img"));
    assert_eq!(p6(&ctx(&sys, &work)).status, Status::Error);
}
