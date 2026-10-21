//! When Execute refuses a platform: before any input is read, and also when the action
//! cache holds a result for the action.

use super::*;

/// A named platform and the code it is refused with.
type Refused = (&'static str, Vec<(&'static str, &'static str)>, Code);

/// Platforms kbf refuses.
fn bad_platforms() -> [Refused; 3] {
    [
        (
            "one requirement twice",
            vec![("OSFamily", "linux"), ("os", "linux")],
            Code::InvalidArgument,
        ),
        (
            "an unknown lease kind",
            vec![("kbf-lease", "vm")],
            Code::InvalidArgument,
        ),
        (
            "an OS no daemon runs",
            vec![("OSFamily", "Windows")],
            Code::FailedPrecondition,
        ),
    ]
}

fn platform_of(props: &[(&str, &str)]) -> Platform {
    Platform {
        properties: props.iter().map(|(n, v)| property(n, v)).collect(),
    }
}

/// An action with its platform in the Action (REAPI 2.2) or, when `in_command`, in the
/// Command, over a Command and an input tree. Returns the action, the command and the
/// input root and the one input (the blobs a refusal must not need).
fn placed(argv: &str, props: &[(&str, &str)], in_command: bool) -> (Blob, Blob, Blob, Blob) {
    let input = Blob::new(format!("input of {argv}"));
    let root = Blob::of(&Directory {
        files: vec![FileNode {
            name: "in".to_owned(),
            digest: Some(input.proto.clone()),
            ..Default::default()
        }],
        ..Default::default()
    });
    #[allow(deprecated)]
    let command = Blob::of(&Command {
        arguments: vec![argv.to_owned()],
        platform: in_command.then(|| platform_of(props)),
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        platform: (!in_command).then(|| platform_of(props)),
        ..Default::default()
    });
    (action, command, root, input)
}

/// Catches: a platform checked only after the input tree is walked, so that a client
/// with a bad platform and inputs the farm does not hold is told MISSING (and uploads
/// them for nothing), and one with inputs the store cannot read is told UNAVAILABLE
/// (and retries forever). The tree is not held here, so a walk would answer MISSING (with
/// details); the Command is held but unreadable (its object is gone), so a read of it
/// would answer UNAVAILABLE. A refusal answers neither.
#[tokio::test]
async fn an_actions_bad_platform_is_refused_before_its_inputs_are_read() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    for (why, props, code) in bad_platforms() {
        let (action, command, ..) = placed(why, &props, false);
        farm.upload(&[&action]).await;
        farm.upload(&[&command]).await;
        farm.delete_object_of(&command).await;
        let status = start(&farm, &action).await.expect_err(why);
        assert_eq!(status.code(), code, "{why}: {status:?}");
        assert!(status.details().is_empty(), "{why}: a read was attempted");
    }
    assert!(script.submitted().is_empty());
}

/// Catches: the same, for a platform that only the Command carries: the Command is
/// read to find it, but the input tree, which is held here yet unreadable (its object
/// is gone), must not be touched; UNAVAILABLE would say it was.
#[tokio::test]
async fn a_commands_bad_platform_is_refused_before_the_input_tree_is_read() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    for (why, props, code) in bad_platforms() {
        let (action, command, root, input) = placed(why, &props, true);
        farm.upload(&[&action, &command]).await;
        farm.upload(&[&root, &input]).await;
        farm.delete_object_of(&root).await;
        let status = start(&farm, &action).await.expect_err(why);
        assert_eq!(status.code(), code, "{why}: {status:?}");
        assert!(status.details().is_empty(), "{why}: nothing to upload");
    }
    assert!(script.submitted().is_empty());
}

/// Catches: a cached result served for an action whose platform kbf refuses (a mistyped
/// property would then work or not by whether the action ran before), for a platform in
/// the Action and for one in the Command; and a refusal that submits.
#[tokio::test]
async fn a_cached_result_does_not_excuse_a_bad_platform() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    for in_command in [false, true] {
        for (why, props, code) in bad_platforms() {
            let (action, command, root, input) = placed(why, &props, in_command);
            farm.upload(&[&action, &command, &root, &input]).await;
            farm.cache
                .write_action_result(Role::Daemon, action.digest, &ActionResult::default())
                .await
                .expect("write");
            let status = start(&farm, &action).await.expect_err(why);
            assert_eq!(
                status.code(),
                code,
                "{why}, command {in_command}: {status:?}"
            );
        }
    }
    assert!(script.submitted().is_empty());
}

/// Catches: the platform check turning a good platform away, or the cache lookup
/// skipped for one: a good platform in the Action, in the Command, and none at all each
/// run (with `skip_cache_lookup`) and each answer from the cache (without it).
#[tokio::test]
async fn a_good_platform_still_runs_and_still_hits_the_cache() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let mut cases = Vec::new();
    for (in_command, props) in [
        (false, vec![("os", "linux")]),
        (true, vec![("os", "linux")]),
    ] {
        cases.push(placed(
            &format!("{props:?} {in_command}"),
            &props,
            in_command,
        ));
    }
    cases.push(placed("no platform", &[], false));
    for (action, command, root, input) in &cases {
        farm.upload(&[action, command, root, input]).await;
        let result = ActionResult {
            exit_code: 7,
            ..Default::default()
        };
        farm.cache
            .write_action_result(Role::Daemon, action.digest, &result)
            .await
            .expect("write");

        let mut ops = start(&farm, action).await.expect("Execute");
        let only = next(&mut ops).await.expect("healthy").expect("an update");
        let answer = response(&only);
        assert!(answer.cached_result);
        assert_eq!(answer.result, Some(result));

        farm.exec()
            .execute(ExecuteRequest {
                action_digest: Some(action.proto.clone()),
                skip_cache_lookup: true,
                ..Default::default()
            })
            .await
            .expect("Execute");
    }
    assert_eq!(script.submitted().len(), cases.len(), "a good platform ran");
}

/// Catches: a cache hit that fails because the action it is for cannot be checked: the
/// action not in the CAS, not decoding, naming no Command, a Command not held, or a
/// Command that does not decode, are answered from the cache (an Execute that runs says
/// what is wrong with them). The closing UNAVAILABLE for an unreadable action only pins
/// the behaviour: the cache lookup before `refuse_platform_of` already fails on it, so
/// that read error in `held` is unreachable apart from the lookup, and the Command-side
/// one is covered by `a_commands_read_error_on_a_cache_hit_is_unavailable`.
#[tokio::test]
async fn a_cached_result_for_an_action_that_cannot_be_checked_is_served() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let garbage = Blob::new(b"\xff\xff\xff not an action".to_vec());
    let held_garbage = Blob::of(&Action {
        command_digest: Some(Blob::new(b"\xff\xff\xff not a command".to_vec()).proto),
        ..Default::default()
    });
    let no_command = Blob::of(&Action::default());
    let malformed_command = Blob::of(&Action {
        command_digest: Some(reapi::Digest {
            hash: "not a hash".to_owned(),
            size_bytes: 1,
        }),
        ..Default::default()
    });
    let absent_command = Blob::of(&Action {
        command_digest: Some(Blob::new(b"never uploaded".to_vec()).proto),
        ..Default::default()
    });
    let bad_command_blob = Blob::new(b"\xff\xff\xff not a command".to_vec());
    let bad_command = Blob::of(&Action {
        command_digest: Some(bad_command_blob.proto.clone()),
        ..Default::default()
    });
    farm.upload(&[
        &garbage,
        &no_command,
        &absent_command,
        &bad_command,
        &bad_command_blob,
    ])
    .await;
    let never_uploaded = Blob::new(b"an action nobody uploaded".to_vec());
    for (why, action) in [
        ("not in the CAS", &never_uploaded),
        ("does not decode", &garbage),
        ("names no Command", &no_command),
        ("names a malformed Command digest", &malformed_command),
        ("Command not held", &absent_command),
        ("Command does not decode", &bad_command),
    ] {
        farm.cache
            .write_action_result(Role::Daemon, action.digest, &ActionResult::default())
            .await
            .expect("write");
        let mut ops = start(&farm, action).await.expect(why);
        let only = next(&mut ops).await.expect("healthy").expect("an update");
        assert!(response(&only).cached_result, "{why}");
    }
    assert!(script.submitted().is_empty());

    farm.upload(&[&held_garbage]).await;
    farm.delete_object_of(&held_garbage).await;
    farm.cache
        .write_action_result(Role::Daemon, held_garbage.digest, &ActionResult::default())
        .await
        .expect("write");
    let status = start(&farm, &held_garbage).await.expect_err("unreadable");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
}

/// Catches: a store error while the Command of a cached action is read to check its
/// platform swallowed (`.ok().flatten()`), serving the cached result. The Action is
/// valid and has no platform, so only the Command can carry one; the Command is held
/// but its object is gone. Nothing before `refuse_platform_of` reads the Command, so
/// only that read can make this UNAVAILABLE.
#[tokio::test]
async fn a_commands_read_error_on_a_cache_hit_is_unavailable() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let (action, command, root, input) = placed("unreadable command", &[], false);
    farm.upload(&[&action]).await;
    farm.upload(&[&command]).await;
    farm.upload(&[&root, &input]).await;
    farm.delete_object_of(&command).await;
    farm.cache
        .write_action_result(Role::Daemon, action.digest, &ActionResult::default())
        .await
        .expect("write");
    let status = start(&farm, &action).await.expect_err("unreadable Command");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert!(script.submitted().is_empty());
}
