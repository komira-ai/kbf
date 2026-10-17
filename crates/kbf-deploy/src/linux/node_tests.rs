//! Rule `node`: the shipped unit passes it, and each defect of the contract in
//! [`super`]'s docs turns it red with its own message.

use kbf_host::testkit::FakeExec;

use super::super::modes::{self, Mode, UnitText, render, render_template};
use super::super::render::{SLICE, Templates, render as render_converged};
use super::*;

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");
const TEMPLATE: &str = include_str!("../../templates/linux/modes/kbf-node.service");

fn config() -> LinuxConfig {
    LinuxConfig::parse(EXAMPLE).expect("valid settings")
}

fn failures(c: &LinuxConfig, text: &str) -> Vec<String> {
    let r = modes::check(c, Mode::Node, text, true).expect("a unit");
    r.failures.iter().map(ToString::to_string).collect()
}

/// Catches a template that breaks its own rule as shipped, and a render that names a
/// different command than [`enroll_command`] (the rule and the template drifting apart).
#[test]
fn the_shipped_unit_passes_and_runs_the_commands_the_rule_names() {
    let c = config();
    let text = render(&c, Some(1500), Mode::Node).expect("renders");
    assert_eq!(failures(&c, &text), Vec::<String>::new());
    assert!(
        text.contains(&format!("\nExecStartPre={}\n", enroll_command(&c))),
        "{text}"
    );
    assert!(
        text.contains("\nExecStart=/usr/local/bin/kbf node --config=/etc/kbf/kbf.toml\n"),
        "{text}"
    );
    assert_eq!(Mode::Node.unit_name(), "kbf-node.service");
    assert_eq!(Mode::Node.name(), "node");
}

const WANT_PRE: &str = "want exactly 'ExecStartPre=/usr/local/bin/kbf enroll --config=/etc/kbf/kbf.toml': enroll is idempotent, and a unit without it (or with a '-' before it) starts a node that never enrolled";
const NOT_ROOT: &str = "not kbf (KBF_USER): the node is unprivileged and its keys are kbf's alone";
const CAUSE: &str = "a node makes its own key pair and is sent the cold store's keys over mTLS, so no unit names a key file";
const CARRIES: &str = "a node holds no key file and no AWS_* variable, the cold store's keys reach a storage node over its mTLS session and stay in its memory (docs/design/install-and-enrollment.md, section 4.3)";

/// Each row plants one defect in the template; the check must fail exactly the rule
/// meant, with this message. `from` must occur once in the template.
#[test]
fn each_planted_defect_turns_its_rule_red() {
    let c = config();
    let user = "User=@KBF_USER@\n";
    let pre = "ExecStartPre=@KBF_BIN_DIR@/kbf enroll --config=@KBF_ETC@/kbf.toml\n";
    let start = "ExecStart=@KBF_BIN_DIR@/kbf node --config=@KBF_ETC@/kbf.toml\n";
    let env = "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n";
    let rows: Vec<(&str, String, String)> = vec![
        (
            user,
            String::new(),
            format!("FAIL node: kbf-node.service runs as root (no User=), {NOT_ROOT}"),
        ),
        (
            user,
            "User=root\n".to_owned(),
            format!("FAIL node: kbf-node.service runs as root, {NOT_ROOT}"),
        ),
        (
            pre,
            String::new(),
            format!("FAIL node: kbf-node.service has no ExecStartPre=, {WANT_PRE}"),
        ),
        (
            "ExecStartPre=@KBF_BIN_DIR@/kbf enroll",
            "ExecStartPre=-@KBF_BIN_DIR@/kbf enroll".to_owned(),
            format!(
                "FAIL node: kbf-node.service has ExecStartPre=-/usr/local/bin/kbf enroll --config=/etc/kbf/kbf.toml, {WANT_PRE}"
            ),
        ),
        (
            pre,
            "ExecStartPre=@KBF_BIN_DIR@/kbf enroll --config=@KBF_ETC@/other.toml\n".to_owned(),
            format!(
                "FAIL node: kbf-node.service has ExecStartPre=/usr/local/bin/kbf enroll --config=/etc/kbf/other.toml, {WANT_PRE}"
            ),
        ),
        (
            start,
            "ExecStart=@KBF_BIN_DIR@/kbf node --config=@KBF_ETC@/other.toml\n".to_owned(),
            "FAIL node: kbf-node.service ExecStart= does not give --config=/etc/kbf/kbf.toml: the node's one config file".to_owned(),
        ),
        (
            start,
            "ExecStart=@KBF_BIN_DIR@/kbf node\n".to_owned(),
            "FAIL node: kbf-node.service ExecStart= does not give --config=/etc/kbf/kbf.toml: the node's one config file".to_owned(),
        ),
        (
            start,
            "ExecStart=@KBF_BIN_DIR@/kbf node --config=@KBF_ETC@/kbf.toml --key=@KBF_ETC@/node.key\n"
                .to_owned(),
            format!("FAIL node: kbf-node.service gives the flag --key: {CAUSE}"),
        ),
        (
            start,
            "ExecStart=@KBF_BIN_DIR@/kbf node --config=@KBF_ETC@/kbf.toml --object-store-keys-file=@KBF_ETC@/s3.env\n"
                .to_owned(),
            format!(
                "FAIL node: kbf-node.service gives the flag --object-store-keys-file: {CAUSE}"
            ),
        ),
        (
            env,
            format!("{env}EnvironmentFile=@KBF_ETC@/s3.env\n"),
            format!(
                "FAIL credential: kbf-node.service carries a key (EnvironmentFile=/etc/kbf/s3.env): {CARRIES}"
            ),
        ),
        (
            env,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@ AWS_ACCESS_KEY_ID=planted\n".to_owned(),
            format!(
                "FAIL credential: kbf-node.service carries a key (Environment= sets AWS_ACCESS_KEY_ID): {CARRIES}"
            ),
        ),
        (
            "KillSignal=SIGTERM\n",
            "KillSignal=SIGINT\n".to_owned(),
            "FAIL stop: kbf-node.service has KillSignal=SIGINT, not KillSignal=SIGTERM: kbf drains on SIGTERM".to_owned(),
        ),
    ];
    for (from, to, want) in rows {
        assert_eq!(TEMPLATE.matches(from).count(), 1, "{from:?} not there once");
        let text = render_template(&c, Some(1500), Mode::Node, &TEMPLATE.replace(from, &to))
            .expect("renders");
        assert_eq!(failures(&c, &text), [want], "{from:?} -> {to:?}");
    }
}

/// Catches a rule that reads only the unit's own file: a drop-in that resets
/// `ExecStartPre=` (systemd applies an empty assignment as "forget the earlier lines") or
/// adds a second one runs a node that skips, or repeats, enrollment.
#[test]
fn drop_ins_change_the_pre_commands_as_systemd_applies_them() {
    let c = config();
    let text = render(&c, Some(1500), Mode::Node).expect("renders");
    let slice = render_converged(&c, None, &Templates::BUILT_IN)
        .expect("renders")
        .slice;
    let check = |unit: UnitText| {
        let r = modes::check_on_host(
            &FakeExec::new(0),
            &c,
            Mode::Node,
            &unit,
            &UnitText::new(SLICE, slice.clone()),
            true,
            None,
        )
        .expect("runs");
        r.failures
            .iter()
            .map(|f| f.message.clone())
            .collect::<Vec<_>>()
    };
    let base = UnitText::new("kbf-node.service", text);
    assert_eq!(check(base.clone()), Vec::<String>::new());
    let reset = base
        .clone()
        .with_drop_in("kbf-node.service.d/a.conf", "[Service]\nExecStartPre=\n");
    assert!(
        check(reset)[0].contains("has no ExecStartPre="),
        "reset drops enrollment"
    );
    let replaced = base.clone().with_drop_in(
        "kbf-node.service.d/a.conf",
        "[Service]\nExecStartPre=\nExecStartPre=/usr/local/bin/kbf enroll --config=/etc/kbf/kbf.toml\n",
    );
    assert_eq!(
        check(replaced),
        Vec::<String>::new(),
        "a reset and the same line"
    );
    let twice = base.with_drop_in(
        "kbf-node.service.d/a.conf",
        "[Service]\nExecStartPre=/usr/bin/true\n",
    );
    assert!(
        check(twice)[0].contains("; /usr/bin/true"),
        "a second pre command"
    );
}
