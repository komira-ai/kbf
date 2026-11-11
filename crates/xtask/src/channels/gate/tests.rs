use super::*;
use kbf_testkit::dir::TestDir;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-gate-{name}"))
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// Runs the gate against an environment of `vars`.
fn run(
    vars: &[(&str, &str)],
    label: &str,
    settings: &[&str],
    requires: &[&str],
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let env = |name: &str| {
        vars.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| (*v).to_owned())
    };
    gate(&env, label, &names(settings), &names(requires), output)
}

#[test]
fn a_job_with_every_setting_is_enabled() {
    // Catches: a gate that keeps a configured channel off.
    let dir = scratch("all");
    let out = dir.join("out");
    let said = run(
        &[("A", "1"), ("B_2", "set")],
        "tap",
        &["A", "B_2"],
        &[],
        Some(&out),
    )
    .expect("enabled");
    assert_eq!(said, ["tap: all its settings are set"]);
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=true\n"
    );
}

#[test]
fn a_job_with_no_setting_is_a_green_notice_that_writes_nothing() {
    // Catches: a channel that runs, or fails, before its owner has made its settings: a
    // variable that is empty and one that is absent are both unset.
    let dir = scratch("none");
    let out = dir.join("out");
    let said = run(&[("A", "")], "tap", &["A", "B"], &[], Some(&out)).expect("a no-op is green");
    assert_eq!(
        said,
        [
            "::notice title=The tap job is off::None of its settings (A, B) is set, so it writes nothing and ends green."
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=false\n"
    );
}

#[test]
fn a_job_with_some_settings_fails_and_names_the_missing_ones_but_never_a_value() {
    // Catches: a typo in one variable name leaving a channel silently off, and a secret's
    // value reaching the message.
    let dir = scratch("some");
    let out = dir.join("out");
    let e = run(
        &[("A", "hunter2"), ("D", "x")],
        "tap",
        &["A", "B", "C", "D"],
        &[],
        Some(&out),
    )
    .unwrap_err();
    assert_eq!(
        e,
        Error::Failed(
            "tap: A, D set but B, C not: set all of them or none, so that a channel is never left half on"
                .to_owned()
        )
    );
    // The later steps are skipped even if a caller ignored the failure.
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=false\n"
    );
}

#[test]
fn the_answer_is_appended_to_the_output_file_and_is_optional() {
    // Catches: an answer that replaces what GITHUB_OUTPUT already holds.
    let dir = scratch("append");
    let out = dir.join("out");
    std::fs::write(&out, "earlier=1\n").expect("seed");
    run(&[("A", "1")], "up", &["A"], &[], Some(&out)).expect("enabled");
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "earlier=1\nenabled=true\n"
    );
    assert!(run(&[("A", "1")], "up", &["A"], &[], None).is_ok());
}

#[test]
fn flags_that_are_not_of_their_form_are_usage_errors() {
    // Catches: a name that is not a variable name, a name given twice, a gate that asks
    // nothing, and a label that could forge a workflow command.
    for (label, settings, requires) in [
        ("tap", vec!["a-b"], vec![]),
        ("tap", vec![""], vec![]),
        ("tap", vec!["A", "A"], vec![]),
        ("tap", vec!["A"], vec!["lower"]),
        ("tap", vec![], vec![]),
        ("", vec!["A"], vec![]),
        ("t\nap", vec!["A"], vec![]),
        ("t::ap", vec!["A"], vec![]),
    ] {
        let e = run(&[("A", "1")], label, &settings, &requires, None).unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{label:?} {settings:?}: {e}");
    }
    let e = run(&[], "tap", &["A", "A"], &[], None).unwrap_err();
    assert_eq!(e, Error::Usage("--setting A is given twice".to_owned()));
}

#[test]
fn a_job_whose_earlier_job_was_off_is_off_and_green_without_judging_its_own_settings() {
    // Catches: the apt index failing "some set" because the read job it follows was off.
    let dir = scratch("require-off");
    let out = dir.join("out");
    let said = run(
        &[
            ("KEY", "set"),
            ("READ_ENABLED", ""),
            ("SIGNED", "true"),
            ("OTHER", "false"),
        ],
        "apt index",
        &["KEY", "FPR"],
        &["READ_ENABLED", "SIGNED", "OTHER"],
        Some(&out),
    )
    .expect("off is green");
    assert_eq!(
        said,
        [
            "::notice title=The apt index job is off::READ_ENABLED, OTHER is not true, so it writes nothing and ends green."
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=false\n"
    );
}

#[test]
fn a_job_whose_earlier_jobs_said_true_is_judged_by_its_own_settings() {
    // Catches: a requirement that switches the settings check off.
    let dir = scratch("require-on");
    let out = dir.join("out");
    let vars = [("KEY", "set"), ("READ_ENABLED", "true")];
    let ok = run(&vars, "apt index", &["KEY"], &["READ_ENABLED"], Some(&out));
    assert_eq!(
        ok.expect("enabled"),
        ["apt index: all its settings are set"]
    );
    let half = run(&vars, "apt index", &["KEY", "FPR"], &["READ_ENABLED"], None);
    assert!(half.unwrap_err().to_string().contains("FPR not"));
}

#[test]
fn a_gate_of_requirements_alone_is_enabled_when_every_one_is_true() {
    // Catches: a job that only follows another being refused for having no settings of its own.
    let said = run(
        &[("READ_ENABLED", "true")],
        "apt write",
        &[],
        &["READ_ENABLED"],
        None,
    )
    .expect("enabled");
    assert_eq!(said, ["apt write: all its settings are set"]);
}

#[test]
fn the_process_environment_is_read_by_name() {
    // Catches: a gate that reads no variable of the process, or one that is not there as set.
    assert!(process_env("PATH").is_some());
    assert_eq!(process_env("XTASK_GATE_A_NAME_NO_ONE_SETS"), None);
}
