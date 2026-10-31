//! The declaration as text: its fields, its tables, and the rules that need nothing but the text.

mod support;

use std::collections::BTreeSet;

use support::{SAMPLE, refusal, sample, with, with_in};
use update_hook::Step;
use update_rights::declaration::{MAX_CEILING_SECS, MAX_SECS};
use update_rights::names::{Capability, Family, PathError, UnitError};
use update_rights::reserved::Violation;
use update_rights::{Declaration, Refusal};

fn caps(names: &[&str]) -> BTreeSet<Capability> {
    names
        .iter()
        .map(|name| Capability::try_from((*name).to_owned()).unwrap())
        .collect()
}

fn field_of(refusal: &Refusal) -> String {
    match refusal {
        Refusal::Value { field, .. } => field.clone(),
        Refusal::Path { field, .. } | Refusal::Reserved { field, .. } => (*field).to_owned(),
        other => panic!("not a field refusal: {other:?}"),
    }
}

/// Catches a loader that drops or renames a field of the design's example.
#[test]
fn the_example_reads_into_every_field() {
    let d = sample();
    assert_eq!(d.name, "tierstore");
    assert_eq!(d.binary, "tierstore");
    assert_eq!(d.hook_args, ["update-hook"]);
    assert_eq!(d.signer.repository, "komira-ai/kbf");
    assert_eq!(d.signer.workflow, "artifacts.yml");
    assert_eq!(d.uid, "tierstore");
    assert_eq!(d.registration_ttl_secs, 300);
    assert_eq!(d.health_ports, BTreeSet::from([9301, 9302]));
    assert_eq!(d.verify_deadline_secs, 1600);
    assert_eq!(d.promotion_ceiling_secs, Some(3600));
    assert_eq!(d.cleanup_retries, 3);
    assert_eq!(d.state_dir, "/var/lib/updater/hooks/tierstore");
    assert_eq!(d.boot.as_deref(), Some("restore"));
    assert_eq!(d.min_version.as_deref(), Some("0.1.0"));
    assert_eq!(d.hook.user, "tierstore-update");
    assert_eq!(d.hook.group, "tierstore-update");
    assert!(d.hook.capabilities.is_empty());
    assert_eq!(d.hook.read_paths.len(), 4);
    assert!(d.hook.write_paths.is_empty());
    assert_eq!(d.hook.units.len(), 2);
    assert_eq!(d.hook.address_families.len(), 4);
    assert!(
        d.hook
            .address_families
            .contains(&Family::try_from("AF_NETLINK".to_owned()).unwrap())
    );
    assert!(d.hook.environment.is_empty());
    assert_eq!(d.hook.timeout_secs.get(Step::Prepare), 600);
    assert_eq!(d.hook.timeout_secs.get(Step::Cutover), 300);
    assert_eq!(d.hook.timeout_secs.get(Step::Verify), 1000);
    assert_eq!(d.hook.timeout_secs.get(Step::Rollback), 600);
    assert_eq!(d.hook.timeout_secs.get(Step::Cleanup), 1300);
    assert_eq!(d.hook.params.len(), 1);
}

/// Catches a default that is not the design's: `hook_args` is `["update-hook"]`, a retry count 3,
/// a registration lifetime 300 s, and everything that is a set starts empty.
#[test]
fn a_minimal_declaration_takes_the_defaults() {
    let text = r#"
name = "svc"
binary = "bin/svc"
signer = { repository = "o/r", workflow = "w.yml" }
uid = "svc"
verify_deadline_secs = 20
state_dir = "/var/lib/updater/hooks/svc"
[hook]
user = "svc-update"
group = "svc-update"
[hook.timeout_secs]
prepare = 1
cutover = 1
verify = 10
rollback = 10
cleanup = 1
"#;
    let d = Declaration::parse(text).unwrap();
    assert_eq!(d.hook_args, ["update-hook"]);
    assert_eq!(d.cleanup_retries, 3);
    assert_eq!(d.registration_ttl_secs, 300);
    assert_eq!(d.promotion_ceiling_secs, None);
    assert_eq!((d.boot, d.min_version), (None, None));
    assert!(d.health_ports.is_empty());
    assert!(d.hook.capabilities.is_empty());
    assert!(d.hook.read_paths.is_empty());
    assert!(d.hook.units.is_empty());
    assert!(d.hook.environment.is_empty());
    assert!(d.hook.params.is_empty());
    for step in Step::ALL {
        assert_eq!(d.hook.step_capabilities.get(step), None);
    }
}

/// Catches `hook_args = []` being replaced by the default: an executable that takes the step
/// first is declared with an empty list.
#[test]
fn an_empty_hook_args_stays_empty() {
    let d = Declaration::parse(&with(
        "name = \"tierstore\"",
        "name = \"tierstore\"\nhook_args = []",
    ))
    .unwrap();
    assert!(d.hook_args.is_empty());
}

/// Catches a step falling back wrongly: a step with no entry holds the hook-wide grant; a step
/// with an empty entry holds nothing.
#[test]
fn a_step_holds_its_own_capabilities_or_the_hook_wide_grant() {
    let wide = with("capabilities = []", "capabilities = [\"CAP_KILL\"]");
    let d = Declaration::parse(&wide).unwrap();
    assert_eq!(
        d.hook.capabilities_for(Step::Prepare),
        &caps(&["CAP_NET_ADMIN"])
    );
    assert!(d.hook.capabilities_for(Step::Verify).is_empty());
    let none = with_in(&wide, "verify = []\n", "");
    let d = Declaration::parse(&none).unwrap();
    assert_eq!(d.hook.capabilities_for(Step::Verify), &caps(&["CAP_KILL"]));
    assert_eq!(d.hook.capabilities_for(Step::Cleanup), &caps(&[]));
}

/// Catches a key that is accepted and ignored: an unknown key in any table refuses the file.
#[test]
fn an_unknown_key_in_any_table_is_refused() {
    for (anchor, extra) in [
        ("name = \"tierstore\"", "name = \"tierstore\"\nsurprise = 1"),
        (
            "workflow = \"artifacts.yml\"",
            "workflow = \"artifacts.yml\", extra = \"x\"",
        ),
        ("[hook]\n", "[hook]\nsurprise = 1\n"),
        (
            "[hook.step_capabilities]\n",
            "[hook.step_capabilities]\nsurprise = []\n",
        ),
        (
            "[hook.timeout_secs]\n",
            "[hook.timeout_secs]\nsurprise = 1\n",
        ),
    ] {
        match refusal(&with(anchor, extra)) {
            Refusal::Syntax(message) => assert!(
                message.contains("surprise") || message.contains("extra"),
                "{message}"
            ),
            other => panic!("{anchor}: {other:?}"),
        }
    }
}

/// Catches a key in the wrong table being read as if it were in the right one: the rights live
/// under `[hook]`, the deadlines at the top.
#[test]
fn a_misplaced_key_is_refused() {
    // A rights key at the top level, a top-level key under [hook].
    let at_top = with(
        "name = \"tierstore\"",
        "name = \"tierstore\"\nread_paths = []",
    );
    assert!(matches!(refusal(&at_top), Refusal::Syntax(_)));
    let under_hook = with("[hook]\n", "[hook]\nverify_deadline_secs = 5000\n");
    assert!(matches!(refusal(&under_hook), Refusal::Syntax(_)));
    // A key after a table header belongs to that table: `state_dir` below `[hook]` is not the
    // top-level one, and the top-level one is then missing.
    let after_header = with_in(
        &with("state_dir = \"/var/lib/updater/hooks/tierstore\"\n", ""),
        "[hook]\n",
        "[hook]\nstate_dir = \"/var/lib/updater/hooks/tierstore\"\n",
    );
    assert!(matches!(refusal(&after_header), Refusal::Syntax(_)));
}

/// Catches text that is not a declaration at all, and a required key left out.
#[test]
fn syntax_and_missing_keys_are_refused() {
    assert!(matches!(refusal("not toml ["), Refusal::Syntax(_)));
    assert!(matches!(refusal(""), Refusal::Syntax(_)));
    assert!(matches!(
        refusal(&with("binary = \"tierstore\"\n", "")),
        Refusal::Syntax(_)
    ));
    assert!(matches!(
        refusal(&with("verify = 1000\n", "")),
        Refusal::Syntax(_)
    ));
    assert!(matches!(
        refusal(&with("prepare = 600\n", "")),
        Refusal::Syntax(_)
    ));
    assert!(matches!(
        refusal(&with("health_ports = [9301, 9302]", "health_ports = [-1]")),
        Refusal::Syntax(_)
    ));
    assert!(matches!(
        refusal(&with(
            "health_ports = [9301, 9302]",
            "health_ports = [70000]"
        )),
        Refusal::Syntax(_)
    ));
    assert!(matches!(
        refusal(&with(
            "\"CAP_NET_ADMIN\"]\ncutover",
            "\"cap_net_admin\"]\ncutover"
        )),
        Refusal::Syntax(_)
    ));
    assert!(matches!(
        refusal(&with("\"AF_UNIX\"", "\"unix\"")),
        Refusal::Syntax(_)
    ));
    assert!(
        Refusal::Syntax("x".into())
            .to_string()
            .contains("not a declaration")
    );
}

/// Catches a name, an account or a word outside its grammar.
#[test]
fn names_and_words_follow_their_grammars() {
    for (from, to, field) in [
        ("name = \"tierstore\"", "name = \"Tier\"", "name"),
        ("binary = \"tierstore\"", "binary = \"../x\"", "binary"),
        ("binary = \"tierstore\"", "binary = \"/x\"", "binary"),
        ("binary = \"tierstore\"", "binary = \"\"", "binary"),
        (
            "repository = \"komira-ai/kbf\"",
            "repository = \"\"",
            "signer.repository",
        ),
        (
            "repository = \"komira-ai/kbf\"",
            "repository = \"a b\"",
            "signer.repository",
        ),
        (
            "workflow = \"artifacts.yml\"",
            "workflow = \"a;b\"",
            "signer.workflow",
        ),
        ("uid = \"tierstore\"", "uid = \"Tier Store\"", "uid"),
        ("user = \"tierstore-update\"", "user = \"\"", "hook.user"),
        (
            "group = \"tierstore-update\"",
            "group = \"1\"",
            "hook.group",
        ),
        ("boot = \"restore\"", "boot = \"a b\"", "boot"),
        (
            "min_version = \"0.1.0\"",
            "min_version = \"\"",
            "min_version",
        ),
    ] {
        assert_eq!(field_of(&refusal(&with(from, to))), field, "{to}");
    }
}

/// Catches `hook_args` carrying an argument systemd-run or the hook could misread, or too many.
#[test]
fn hook_args_are_plain_words() {
    let at = |args: &str| {
        with(
            "name = \"tierstore\"",
            &format!("name = \"tierstore\"\nhook_args = {args}"),
        )
    };
    assert!(Declaration::parse(&at("[\"update-hook\", \"--a=b\"]")).is_ok());
    for bad in ["[\"a b\"]", "[\"\"]", "[\"$x\"]", "[\"a\\\\n\"]"] {
        assert_eq!(field_of(&refusal(&at(bad))), "hook_args", "{bad}");
    }
    let nine = format!("[{}]", ["\"a\""; 9].join(","));
    assert_eq!(field_of(&refusal(&at(&nine))), "hook_args");
    let eight = format!("[{}]", ["\"a\""; 8].join(","));
    assert!(Declaration::parse(&at(&eight)).is_ok());
}

/// Catches a bound outside its range: zero (a step with no time), a day and a second, a port 0,
/// too many retries, a ceiling past a week.
#[test]
fn numbers_stay_within_bounds() {
    let day = MAX_SECS.to_string();
    let over = (MAX_SECS + 1).to_string();
    // The deadline is raised with the timeouts so only the bound under test can fire.
    let deadline = |n: &str| {
        with(
            "verify_deadline_secs = 1600",
            &format!("verify_deadline_secs = {n}"),
        )
    };
    assert!(Declaration::parse(&deadline(&day)).is_ok());
    assert_eq!(field_of(&refusal(&deadline(&over))), "verify_deadline_secs");
    assert_eq!(field_of(&refusal(&deadline("0"))), "verify_deadline_secs");
    for (anchor, step) in [
        ("prepare = 600", "prepare"),
        ("cutover = 300", "cutover"),
        ("verify = 1000", "verify"),
        ("rollback = 600", "rollback"),
        ("cleanup = 1300", "cleanup"),
    ] {
        for n in ["0", over.as_str()] {
            let text = with_in(SAMPLE, anchor, &format!("{step} = {n}"));
            assert_eq!(
                field_of(&refusal(&text)),
                format!("hook.timeout_secs.{step}")
            );
        }
    }
    assert_eq!(
        field_of(&refusal(&with(
            "registration_ttl_secs = 300",
            "registration_ttl_secs = 0"
        ))),
        "registration_ttl_secs"
    );
    assert_eq!(
        field_of(&refusal(&with(
            "registration_ttl_secs = 300",
            &format!("registration_ttl_secs = {over}")
        ))),
        "registration_ttl_secs"
    );
    assert_eq!(
        field_of(&refusal(&with("[9301, 9302]", "[0, 9302]"))),
        "health_ports"
    );
    assert_eq!(
        field_of(&refusal(&with(
            "cleanup_retries = 3",
            "cleanup_retries = 11"
        ))),
        "cleanup_retries"
    );
    assert!(Declaration::parse(&with("cleanup_retries = 3", "cleanup_retries = 10")).is_ok());
    assert!(Declaration::parse(&with("cleanup_retries = 3", "cleanup_retries = 0")).is_ok());
    let ceiling = |n: &str| {
        with(
            "promotion_ceiling_secs = 3600",
            &format!("promotion_ceiling_secs = {n}"),
        )
    };
    assert!(Declaration::parse(&ceiling(&MAX_CEILING_SECS.to_string())).is_ok());
    assert_eq!(
        field_of(&refusal(&ceiling(&(MAX_CEILING_SECS + 1).to_string()))),
        "promotion_ceiling_secs"
    );
    assert_eq!(field_of(&refusal(&ceiling("0"))), "promotion_ceiling_secs");
}

/// Catches a verify deadline that would roll back a verify still allowed to run: it must be at
/// least the `verify` bound plus the `rollback` bound (1000 + 600 here).
#[test]
fn the_deadline_covers_verify_and_rollback() {
    let at = |n: u64| {
        with(
            "verify_deadline_secs = 1600",
            &format!("verify_deadline_secs = {n}"),
        )
    };
    assert!(Declaration::parse(&at(1600)).is_ok());
    assert!(Declaration::parse(&at(1601)).is_ok());
    for deadline in [1599, 1000, 600, 1] {
        assert_eq!(
            refusal(&at(deadline)),
            Refusal::Deadline {
                deadline,
                needed: 1600
            },
            "{deadline}"
        );
    }
    assert!(refusal(&at(1599)).to_string().contains("1599"));
}

/// Catches a path that systemd would read as something else, in each place a path is written.
#[test]
fn paths_follow_the_grammar_everywhere() {
    for (from, field) in [
        ("\"/var/lib/updater/hooks/tierstore\"", "state_dir"),
        ("\"/usr/sbin/nft\"", "hook.read_paths"),
        ("write_paths = []", "hook.write_paths"),
    ] {
        for bad in [
            "-/x", "+/x", "!/x", "x/y", "/a/../b", "/a b", "/a%h", "/a//b",
        ] {
            let to = match field {
                "hook.write_paths" => format!("write_paths = [\"{bad}\"]"),
                _ => format!("\"{bad}\""),
            };
            match refusal(&with(from, &to)) {
                Refusal::Path {
                    field: found, path, ..
                } => {
                    assert_eq!((found, path.as_str()), (field, bad));
                }
                other => panic!("{field} {bad}: {other:?}"),
            }
        }
    }
    match refusal(&with("\"/etc/tierstore\"", "\"-/etc/tierstore\"")) {
        Refusal::Path { why, .. } => assert_eq!(why, PathError::SystemdPrefix('-')),
        other => panic!("{other:?}"),
    }
}

/// Catches the deny list not being applied to each place a path is written.
#[test]
fn reserved_paths_are_refused_everywhere() {
    for (from, to, field, why) in [
        (
            "\"/var/lib/updater/hooks/tierstore\"",
            "\"/etc\"",
            "state_dir",
            Violation::Names("/etc"),
        ),
        (
            "\"/var/lib/updater/hooks/tierstore\"",
            "\"/var/lib\"",
            "state_dir",
            Violation::Contains("/var/lib/apt"),
        ),
        (
            "\"/var/lib/updater/hooks/tierstore\"",
            "\"/var/lib/updater/other\"",
            "state_dir",
            Violation::Inside("/var/lib/updater"),
        ),
        (
            "\"/usr/sbin/nft\"",
            "\"/\"",
            "hook.read_paths",
            Violation::Names("/"),
        ),
        (
            "\"/usr/sbin/nft\"",
            "\"/usr\"",
            "hook.read_paths",
            Violation::Names("/usr"),
        ),
        (
            "\"/usr/sbin/nft\"",
            "\"/etc/updater.d\"",
            "hook.read_paths",
            Violation::Names("/etc/updater.d"),
        ),
        (
            "write_paths = []",
            "write_paths = [\"/etc/updater.d/x.toml\"]",
            "hook.write_paths",
            Violation::Inside("/etc"),
        ),
        (
            "write_paths = []",
            "write_paths = [\"/etc/sudoers.d\"]",
            "hook.write_paths",
            Violation::Names("/etc/sudoers.d"),
        ),
        (
            "write_paths = []",
            "write_paths = [\"/root\"]",
            "hook.write_paths",
            Violation::Names("/root"),
        ),
        (
            "write_paths = []",
            "write_paths = [\"/boot\"]",
            "hook.write_paths",
            Violation::Names("/boot"),
        ),
    ] {
        match refusal(&with(from, to)) {
            Refusal::Reserved {
                field: found,
                why: found_why,
                ..
            } => {
                assert_eq!(found, field, "{to}");
                match (&found_why, &why) {
                    (Violation::Inside(found), Violation::Inside(expected)) => {
                        assert!(
                            update_rights::reserved::is_within(found, expected)
                                || found == expected,
                            "{to}"
                        );
                    }
                    (found, expected) => assert_eq!(found, expected, "{to}"),
                }
            }
            other => panic!("{to}: {other:?}"),
        }
    }
    // A single file of a reserved directory is allowed to be read, and only to be read.
    assert!(
        Declaration::parse(SAMPLE)
            .unwrap()
            .hook
            .read_paths
            .contains("/etc/updater.d/tierstore.toml")
    );
    assert!(matches!(
        refusal(&with(
            "write_paths = []",
            "write_paths = [\"/etc/tierstore\"]"
        )),
        Refusal::Reserved {
            why: Violation::Inside(_),
            ..
        }
    ));
}

/// Catches a unit the service does not own, and a unit name that is no unit.
#[test]
fn units_are_the_services_own() {
    for (unit, why) in [
        ("sshd.service", UnitError::NotTheServices),
        ("tierstore.timer", UnitError::NotService),
        ("tierstore@a b.service", UnitError::BadInstance),
    ] {
        let text = with("\"tierstore@9301.service\"", &format!("\"{unit}\""));
        assert_eq!(
            refusal(&text),
            Refusal::Unit {
                unit: unit.to_owned(),
                why
            }
        );
    }
    assert!(
        refusal(&with("\"tierstore@9301.service\"", "\"sshd.service\""))
            .to_string()
            .contains("sshd.service")
    );
}

/// Catches an environment that systemd would split or expand, or a name that is no variable.
#[test]
fn the_environment_is_plain() {
    let at = |env: &str| with("environment = { }", &format!("environment = {env}"));
    assert!(Declaration::parse(&at("{ A = \"b\", C_1 = \"/x:y\" }")).is_ok());
    for bad in [
        "{ \"A B\" = \"x\" }",
        "{ A = \"x y\" }",
        "{ A = \"$HOME\" }",
        "{ \"1A\" = \"x\" }",
        "{ A = \"%h\" }",
    ] {
        assert_eq!(field_of(&refusal(&at(bad))), "hook.environment", "{bad}");
    }
}

/// Catches the params table accepting what the contract refuses: a table, an array, an
/// `updater.` key; and refusing what it accepts: numbers, booleans, strings.
#[test]
fn params_are_scalars_and_not_the_updaters() {
    let at = |params: &str| with("promotion_bound_secs = 900", params);
    assert!(Declaration::parse(&at("a = 1\nb = true\nc = \"s\"\nd = 1.5")).is_ok());
    assert_eq!(field_of(&refusal(&at("a = [1]"))), "hook.params");
    assert_eq!(field_of(&refusal(&at("a = { b = 1 }"))), "hook.params");
    assert_eq!(
        field_of(&refusal(&at("\"updater.promotion_ceiling_secs\" = 5"))),
        "hook.params"
    );
    assert_eq!(
        field_of(&refusal(&at("\"updater.anything\" = 5"))),
        "hook.params"
    );
    assert_eq!(field_of(&refusal(&at("updater.x = 5"))), "hook.params");
}

/// Catches a string parameter reaching a hook that runs as root: the design allows strings only
/// for keys a schema lists, and none exists, so a root hook takes numbers and booleans. A hook
/// that is not root keeps its strings.
#[test]
fn a_root_hook_takes_no_string_parameter() {
    let root = |params: &str| {
        with_in(
            &with("promotion_bound_secs = 900", params),
            "user = \"tierstore-update\"",
            "user = \"root\"",
        )
    };
    assert!(Declaration::parse(&root("a = 1\nb = true\nc = 1.5")).is_ok());
    assert_eq!(field_of(&refusal(&root("c = \"s\""))), "hook.params");
    assert_eq!(field_of(&refusal(&root("a = 1\nz = \"\""))), "hook.params");
    let plain = with("promotion_bound_secs = 900", "c = \"s\"");
    assert!(Declaration::parse(&plain).is_ok());
}

/// Every refusal says what it is about.
#[test]
fn refusals_read_as_sentences() {
    for refusal in [
        Refusal::File("f".into()),
        Refusal::DirectoryUnsafe("d".into()),
        Refusal::Account("a".into()),
        Refusal::StateDirWritable { path: "p".into() },
        Refusal::Conflict {
            what: "a unit",
            with: "other".into(),
        },
    ] {
        assert!(!refusal.to_string().is_empty());
    }
}

/// Catches a write path outside the allowed roots getting through the text rules: `/srv`, `/home`
/// and `/var/log` are refused with the reason; one component under `/opt` is not.
#[test]
fn a_write_path_outside_the_write_roots_is_refused() {
    let at = |path: &str| with("write_paths = []", &format!("write_paths = [\"{path}\"]"));
    for path in ["/srv/x", "/mnt/x", "/var/log/x", "/opt"] {
        match refusal(&at(path)) {
            Refusal::Reserved { field, why, .. } => {
                assert_eq!(
                    (field, why),
                    ("hook.write_paths", Violation::NotAllowed),
                    "{path}"
                );
            }
            other => panic!("{path}: {other:?}"),
        }
    }
    for path in ["/opt/x", "/var/lib/x/y", "/var/cache/x", "/run/x"] {
        assert!(Declaration::parse(&at(path)).is_ok(), "{path}");
    }
    // The state directory and the read paths are not held to the list.
    assert!(Declaration::parse(&with("\"/etc/tierstore\"", "\"/srv/x\"")).is_ok());
}

/// Catches an environment variable that changes what a program loads or resolves: the loader's
/// (`LD_*`), the character conversion and locale search paths, the allocator's (`MALLOC_*`), the
/// resolver's, and a `PATH` that is not absolute directories only. Harmless names stay.
#[test]
fn the_environment_cannot_hijack_the_runtime() {
    let at = |env: &str| with("environment = { }", &format!("environment = {env}"));
    for name in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "LD_DEBUG",
        "LD_BIND_NOW",
        "GCONV_PATH",
        "LOCPATH",
        "NLSPATH",
        "MALLOC_CONF",
        "MALLOC_ARENA_MAX",
        "MALLOC_PERTURB_",
        "HOSTALIASES",
        "RES_OPTIONS",
        "GLIBC_TUNABLES",
        "OPENSSL_CONF",
        "OPENSSL_ENGINES",
        "OPENSSL_MODULES",
        "SSLKEYLOGFILE",
        "ZDOTDIR",
        "GIT_SSH_COMMAND",
        "GIT_EXEC_PATH",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_ASKPASS",
        // What a script run by the hook would load or evaluate before its own text.
        "BASH_ENV",
        "ENV",
        "IFS",
        "CDPATH",
        "SHELLOPTS",
        "BASHOPTS",
        "PS4",
        "PROMPT_COMMAND",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "PYTHONHOME",
        "PYTHONINSPECT",
        "PYTHONWARNINGS",
        "PERL5LIB",
        "PERL5OPT",
        "PERLLIB",
        "PERL5DB",
        "RUBYLIB",
        "RUBYOPT",
        "NODE_OPTIONS",
        "NODE_PATH",
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "CLASSPATH",
        "LUA_PATH",
        "LUA_CPATH",
        "TCLLIBPATH",
        "PHPRC",
        "PHP_INI_SCAN_DIR",
        "GEM_PATH",
        "GEM_HOME",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
    ] {
        let text = at(&format!("{{ {name} = \"/x\" }}"));
        assert_eq!(field_of(&refusal(&text)), "hook.environment", "{name}");
    }
    for path in [
        "bin",
        "/usr/bin:bin",
        "",
        ":/usr/bin",
        "/usr/bin:",
        "/usr/bin:.",
        "/usr/../bin",
        "/",
    ] {
        let text = at(&format!("{{ PATH = \"{path}\" }}"));
        let expected = path == "/";
        // `/` alone is an absolute directory.
        assert_eq!(Declaration::parse(&text).is_ok(), expected, "PATH={path:?}");
    }
    for ok in [
        "{ PATH = \"/usr/bin:/usr/sbin\" }",
        "{ PATH = \"/opt/x/bin\" }",
        "{ TMPDIR = \"/run/x\", LANG = \"C\", TZ = \"UTC\" }",
        "{ LDX = \"1\", LDAP_URI = \"x\", MALLOCX = \"1\" }",
        "{ GITHUB_TOKEN = \"x\", ENVIRONMENT = \"x\", IFS_X = \"1\", NODE_ENV = \"production\" }",
    ] {
        assert!(Declaration::parse(&at(ok)).is_ok(), "{ok}");
    }
}
