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
    args: Args,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let env = |name: &str| {
        vars.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| (*v).to_owned())
    };
    gate(
        &env,
        label,
        &names(args.settings),
        &names(args.secrets),
        &names(args.requires),
        output,
    )
}

/// The `--setting`, `--secret` and `--require` arguments of one run.
#[derive(Default)]
struct Args<'a> {
    settings: &'a [&'a str],
    secrets: &'a [&'a str],
    requires: &'a [&'a str],
}

#[test]
fn a_job_with_every_setting_and_secret_is_enabled() {
    // Catches: a gate that keeps a configured channel off.
    let dir = scratch("all");
    let out = dir.join("out");
    let said = run(
        &[("KEY", "1")],
        "tap",
        Args {
            settings: &["A=1", "B_2=set"],
            secrets: &["KEY"],
            ..Args::default()
        },
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
    // Catches: a channel that runs, or fails, before its owner has made its settings: a value
    // that is empty and a secret that is absent are both unset.
    let dir = scratch("none");
    let out = dir.join("out");
    let said = run(
        &[("KEY", "")],
        "tap",
        Args {
            settings: &["A=", "B="],
            secrets: &["KEY", "OTHER"],
            ..Args::default()
        },
        Some(&out),
    )
    .expect("a no-op is green");
    assert_eq!(
        said,
        [
            "::notice title=The tap job is off::None of its settings (A, B, KEY, OTHER) is set, so it writes nothing and ends green."
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=false\n"
    );
}

#[test]
fn a_job_with_some_settings_fails_and_names_the_missing_ones_but_never_a_value() {
    // Catches: a typo in one variable name leaving a channel silently off, and a value
    // reaching the message.
    let dir = scratch("some");
    let out = dir.join("out");
    let e = run(
        &[("D", "x")],
        "tap",
        Args {
            settings: &["A=hunter2", "B=", "C="],
            secrets: &["D"],
            ..Args::default()
        },
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
fn a_setting_is_never_read_from_the_environment() {
    // Catches: a setting taken from a variable of the same name (configuration through the
    // environment) when its value on the command line is empty.
    let e = run(
        &[("A", "from-env")],
        "tap",
        Args {
            settings: &["A="],
            secrets: &["S"],
            ..Args::default()
        },
        None,
    )
    .expect("every one unset is green");
    assert_eq!(
        e,
        [
            "::notice title=The tap job is off::None of its settings (A, S) is set, so it writes nothing and ends green."
        ]
    );
}

#[test]
fn the_answer_is_appended_to_the_output_file_and_is_optional() {
    // Catches: an answer that replaces what GITHUB_OUTPUT already holds.
    let dir = scratch("append");
    let out = dir.join("out");
    std::fs::write(&out, "earlier=1\n").expect("seed");
    let args = || Args {
        settings: &["A=1"],
        ..Args::default()
    };
    run(&[], "up", args(), Some(&out)).expect("enabled");
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "earlier=1\nenabled=true\n"
    );
    assert!(run(&[], "up", args(), None).is_ok());
}

#[test]
fn flags_that_are_not_of_their_form_are_usage_errors() {
    // Catches: a name that is not a variable name, a setting or requirement without `=`, a
    // name given twice (also across flags), a gate that asks nothing, and a label that could
    // forge a workflow command.
    let none = Vec::<&str>::new;
    for (label, settings, secrets, requires) in [
        ("tap", vec!["a-b=1"], none(), none()),
        ("tap", vec!["=1"], none(), none()),
        ("tap", vec!["A"], none(), none()),
        ("tap", vec!["A=1", "A=2"], none(), none()),
        ("tap", vec!["A=1"], vec!["A"], none()),
        ("tap", vec![], vec!["S", "S"], none()),
        ("tap", vec![], vec!["lower"], none()),
        ("tap", vec![], vec![""], none()),
        ("tap", vec![], none(), vec!["lower=true"]),
        ("tap", vec![], none(), vec!["R"]),
        ("tap", vec![], none(), vec!["R=true", "R=true"]),
        ("tap", vec!["R=1"], none(), vec!["R=true"]),
        ("tap", vec![], none(), none()),
        ("", vec!["A=1"], none(), none()),
        ("t\nap", vec!["A=1"], none(), none()),
        ("t::ap", vec!["A=1"], none(), none()),
    ] {
        let args = Args {
            settings: &settings,
            secrets: &secrets,
            requires: &requires,
        };
        let e = run(&[("S", "1")], label, args, None).unwrap_err();
        assert!(
            matches!(e, Error::Usage(_)),
            "{label:?} {settings:?} {secrets:?} {requires:?}: {e}"
        );
    }
    let two = Args {
        settings: &["A=1", "A=2"],
        ..Args::default()
    };
    let e = run(&[], "tap", two, None).unwrap_err();
    assert_eq!(e, Error::Usage("--setting A is given twice".to_owned()));
    let e = run(
        &[],
        "tap",
        Args {
            secrets: &["S", "S"],
            ..Args::default()
        },
        None,
    )
    .unwrap_err();
    assert_eq!(e, Error::Usage("--secret S is given twice".to_owned()));
}

#[test]
fn a_value_is_not_echoed_when_the_form_is_wrong() {
    // Catches: a value (a secret passed by mistake as `--setting`) reaching the usage message.
    let e = run(
        &[],
        "tap",
        Args {
            settings: &["hunter2"],
            ..Args::default()
        },
        None,
    )
    .unwrap_err();
    assert_eq!(
        e,
        Error::Usage("--setting is not NAME=VALUE (a name, `=`, a value)".to_owned())
    );
}

#[test]
fn a_job_whose_earlier_job_was_off_is_off_and_green_without_judging_its_own_settings() {
    // Catches: the apt index failing "some set" because the read job it follows was off.
    let dir = scratch("require-off");
    let out = dir.join("out");
    let said = run(
        &[("KEY", "set")],
        "apt index",
        Args {
            settings: &["FPR="],
            secrets: &["KEY"],
            requires: &["READ_ENABLED=", "SIGNED=true", "OTHER=false"],
        },
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
    let ok = run(
        &[],
        "apt index",
        Args {
            settings: &["KEY=set"],
            requires: &["READ_ENABLED=true"],
            ..Args::default()
        },
        Some(&out),
    );
    assert_eq!(
        ok.expect("enabled"),
        ["apt index: all its settings are set"]
    );
    let half = run(
        &[],
        "apt index",
        Args {
            settings: &["KEY=set", "FPR="],
            requires: &["READ_ENABLED=true"],
            ..Args::default()
        },
        None,
    );
    assert!(half.unwrap_err().to_string().contains("FPR not"));
}

#[test]
fn a_gate_of_requirements_alone_is_enabled_when_every_one_is_true() {
    // Catches: a job that only follows another being refused for having no settings of its own.
    let said = run(
        &[],
        "apt write",
        Args {
            requires: &["READ_ENABLED=true"],
            ..Args::default()
        },
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
