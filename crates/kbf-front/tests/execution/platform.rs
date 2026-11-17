//! What Execute reads from the platform: the lease kind, its name in any case.

use super::*;

/// Catches: a lease kind read from the wrong place (REAPI 2.2 clients put the platform
/// in the Action, older ones in the Command), including an Action whose platform is
/// present but empty hiding the Command's; an unknown kind run as a shared action; and
/// a platform with a repeated property accepted.
#[tokio::test]
async fn the_lease_kind_comes_from_the_platform() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;

    let in_action = job("mac", &[("kbf-lease", "whole_machine")], true);
    farm.upload(&in_action.blobs.iter().collect::<Vec<_>>())
        .await;
    start(&farm, &in_action.action).await.expect("Execute");

    // An older client: the Action has no platform, the Command has one.
    let older = job("old", &[], false);
    #[allow(deprecated)]
    let command = Blob::of(&Command {
        arguments: vec!["old".to_owned()],
        platform: Some(Platform {
            properties: vec![property("kbf-lease", "whole_machine")],
        }),
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(older.blobs[2].proto.clone()),
        ..Default::default()
    });
    farm.upload(&[&action, &command, &older.blobs[2], &older.blobs[3]])
        .await;
    start(&farm, &action).await.expect("Execute");
    // The same, but the Action carries an empty platform rather than none.
    #[allow(deprecated)]
    let command = Blob::of(&Command {
        arguments: vec!["old, empty platform".to_owned()],
        platform: Some(Platform {
            properties: vec![property("kbf-lease", "whole_machine")],
        }),
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(older.blobs[2].proto.clone()),
        platform: Some(Platform::default()),
        ..Default::default()
    });
    farm.upload(&[&action, &command]).await;
    start(&farm, &action).await.expect("Execute");

    let submitted = script.submitted();
    assert!(submitted[0].whole_machine);
    assert!(submitted[1].whole_machine, "the Command's platform ignored");
    assert!(
        submitted[2].whole_machine,
        "an empty Action platform hid the Command's"
    );

    for (why, props) in [
        ("an unknown kind", vec![("kbf-lease", "vm")]),
        (
            "a repeated property",
            vec![("kbf-lease", "action"), ("kbf-lease", "action")],
        ),
    ] {
        let bad = job(why, &props, false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(why);
        assert_eq!(status.code(), Code::InvalidArgument, "{why}");
    }
    assert_eq!(script.submitted().len(), 3);

    for (why, props) in [("none", vec![]), ("action", vec![("kbf-lease", "action")])] {
        let shared = job(why, &props, false);
        farm.upload(&shared.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &shared.action).await.expect(why);
        let submitted = script.submitted();
        assert!(!submitted.last().expect("submitted").whole_machine, "{why}");
    }
}

/// Catches: the lease property ignored because of its case, so `KBF-LEASE=whole_machine`
/// runs beside other work; and it sent in two spellings resolved silently by order.
#[tokio::test]
async fn the_lease_property_name_is_read_in_any_case() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let props = [
        ("osfamily", "linux"),
        ("KBF-Lease", "whole_machine"),
        ("Pool", "default"),
    ];
    let any_case = job("any case", &props, false);
    farm.upload(&any_case.blobs.iter().collect::<Vec<_>>())
        .await;
    start(&farm, &any_case.action).await.expect("Execute");
    let [submitted] = script.submitted().try_into().expect("one submission");
    assert!(submitted.whole_machine);

    let props = [("kbf-lease", "action"), ("Kbf-Lease", "whole_machine")];
    let bad = job("two spellings", &props, false);
    farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
    let status = start(&farm, &bad.action).await.expect_err("two spellings");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("given twice"), "{status:?}");
    assert_eq!(script.submitted().len(), 1);
}
