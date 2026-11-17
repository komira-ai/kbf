//! The size class `Front::action` reads from an action's Command.

use super::Rig;
use crate::executor::Inputs as _;

/// A configuration hash buck2 puts after the cell.
const CFG: &str = "7a3c1e9f0b2d4c68";

/// The class `Front::action` gives the action whose Command `command` fills.
async fn class_of(command: impl FnOnce(&mut kbf_proto::reapi::Command)) -> String {
    let rig = Rig::new();
    let digest = rig.action(|_| {}, command).await;
    let facts = rig.front.action(&digest).await.expect("facts");
    facts.size_class.as_str().to_owned()
}

/// Asserts that an action's class is read from the Command the farm stored for it: a compile
/// whose output paths are under its target's `buck-out` directory is `<target> build`, and a
/// test run of that target (its program the test binary buck2 built, its outputs under
/// `test/execution`) is `<target> test`. Catches a class taken from anything but the Command
/// (the action digest, the image, the client's metadata), the two kinds sharing one class, and
/// an action of a target sized by a class of no target.
#[tokio::test]
async fn a_compile_and_a_test_run_of_a_target_are_its_build_and_test_classes() {
    let dir = format!("buck-out/v2/art/root/{CFG}/crates/kbf-server/__kbf-server__");
    let out = format!("{dir}/lib.rlib");
    let built = class_of(|command| {
        command.arguments = vec!["rustc".to_owned(), "src/lib.rs".to_owned()];
        command.output_paths = vec![out];
    })
    .await;
    assert_eq!(built, "root.crates.kbf-server.kbf-server.build");
    let binary = format!("{dir}/kbf-server-test");
    let run = class_of(|command| {
        command.arguments = vec![binary];
        command.output_paths = vec![
            "buck-out/v2/test/execution/root/0123456789abcdef/fedcba9876543210/default/o"
                .to_owned(),
        ];
    })
    .await;
    assert_eq!(run, "root.crates.kbf-server.kbf-server.test");
}

/// Asserts that a client older than REAPI 2.1, which names its outputs only in `output_files`
/// and `output_directories`, gets the class it would get naming them in `output_paths`. Catches
/// a class read from `output_paths` alone, which would size such a client's every action by the
/// class of its program only.
#[tokio::test]
#[allow(deprecated)] // `output_files`, `output_directories`: what such a client sends.
async fn outputs_named_the_old_way_give_the_same_class() {
    let out = format!("buck-out/v2/art/root/{CFG}/pkg/__t__/o");
    let new = class_of(|command| {
        command.arguments = vec!["cc".to_owned()];
        command.output_paths = vec![out.clone()];
    })
    .await;
    let files = class_of(|command| {
        command.arguments = vec!["cc".to_owned()];
        command.output_files = vec![out.clone()];
    })
    .await;
    let dirs = class_of(|command| {
        command.arguments = vec!["cc".to_owned()];
        command.output_directories = vec![out.clone()];
    })
    .await;
    assert_eq!(new, "root.pkg.t.build");
    assert_eq!(
        (files.as_str(), dirs.as_str()),
        (new.as_str(), new.as_str())
    );
}
