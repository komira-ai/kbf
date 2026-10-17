//! What the `action usage:` line of an operation says, after a farm takes the store over.
//! The line is keyed and labelled by what the callers described when they submitted
//! (`kbf_server::action_usage::Subject`); a successor was not there, so the store has to
//! carry it. Two farms run over one store, as in `fate.rs`.

use kbf_front::ActionLabel;
use kbf_server::action_usage::Subject;

use super::*;
use crate::thread_log::logged;

/// What a buck2 Execute of a compile says: argv, the output path that names the
/// target, and the mnemonic and target its `RequestMetadata` sent.
fn described() -> Described {
    Described {
        arguments: vec!["/usr/bin/clang".to_owned(), "-c".to_owned()],
        outputs: vec!["buck-out/v2/art/root/0123456789abcdef/pkg/__t__/out.o".to_owned()],
        label: ActionLabel {
            mnemonic: Some("CxxCompile".to_owned()),
            target: Some("root//pkg:t".to_owned()),
        },
    }
}

fn submit_described(farm: &TestFarm) {
    farm.submit(Submission {
        request: request(1),
        call: None,
        described: described(),
    })
    .map(drop)
    .expect("submitted");
}

/// A result of `lease` that carries a measured `ResourceUsage`, so the farm logs the
/// operation's `action usage:` line when it takes it.
fn measured(lease: LeaseId) -> worker::Result {
    let usage = worker::ResourceUsage {
        wall_micros: 1_000,
        ..Default::default()
    };
    let result = kbf_proto::reapi::ActionResult {
        execution_metadata: Some(kbf_proto::reapi::ExecutedActionMetadata {
            auxiliary_metadata: vec![usage.to_any()],
            ..Default::default()
        }),
        ..Default::default()
    };
    worker::Result {
        action_result: Some(result),
        ..invalid(lease)
    }
}

/// The successor takes the operation up, grants it to a node and takes its measured
/// result. Returns the `action usage:` lines it logged.
fn usage_lines(successor: &TestFarm) -> Vec<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime");
    let text = logged(|| {
        runtime.block_on(async {
            let (_, mut inbox) = register(successor);
            let [lease] = starts(&mut inbox)[..] else {
                panic!("one lease expected");
            };
            let ack = successor.report(&node(), measured(lease)).await;
            assert!(ack.expect("an acknowledgement").accepted);
        });
    });
    text.lines()
        .filter(|line| line.contains("action usage:"))
        .map(str::to_owned)
        .collect()
}

/// The line's words that name the subject must be the described action's.
fn assert_subject_named(lines: &[String]) {
    let (subject, _) = Subject::of(described());
    assert!(subject.target.is_some(), "the described paths name a target");
    let [line] = lines else {
        panic!("one usage line expected: {lines:?}");
    };
    for word in [
        "mnemonic=\"CxxCompile\"".to_owned(),
        "target_hint=\"root//pkg:t\"".to_owned(),
        format!("key={}", subject.key),
        "cfg=0123456789abcdef".to_owned(),
        "target=\"root//pkg:t\"".to_owned(),
        "target_kind=target".to_owned(),
    ] {
        assert!(
            line.split(' ').any(|w| w == word),
            "{word} missing from {line}"
        );
    }
}

/// Catches: a recovered operation whose usage line names the empty shape (`mnemonic=-`,
/// `target=-`, the key of no program), so an action that ran across a handover cannot be
/// followed from build to build. Mutant: the subject row not written.
#[test]
fn a_handed_over_operation_keeps_its_mnemonic_and_target() {
    let disk = Disk::new();
    let old = disk.generation_farm();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime");
    runtime.block_on(async { submit_described(&old) });
    old.fence();
    let successor = disk.generation_farm();
    assert_subject_named(&usage_lines(&successor));
}

/// Catches: the same, when the old process dies without a handover.
#[test]
fn a_crashed_operation_keeps_its_mnemonic_and_target() {
    let mut disk = Disk::new();
    let old = disk.generation_farm();
    submit_described(&old);
    disk.crash(old);
    let successor = disk.generation_farm();
    assert_subject_named(&usage_lines(&successor));
}

/// Catches: a subject row left behind when the operation ends, which would grow with
/// every submission. One row while the operation is unfinished; none once it finishes.
#[tokio::test(start_paused = true)]
async fn the_subject_row_goes_when_the_operation_finishes() {
    let disk = Disk::new();
    let farm = disk.generation_farm();
    let (_, mut inbox) = register(&farm);
    submit_described(&farm);
    assert_eq!(disk.rows("subject/"), 1, "a submission left no subject row");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert!(accepted(&farm, lease).await);
    assert_eq!(disk.rows("op/"), 0);
    assert_eq!(disk.rows("subject/"), 0, "the subject outlived its operation");
}
