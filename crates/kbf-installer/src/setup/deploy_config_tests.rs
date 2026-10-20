//! Tests of the `deploy.json` that `setup` writes for `kbf-installer deploy` (gap 6): the file
//! against its golden and through deploy's own reader, the defaults taken from `installer.toml`,
//! a refusal for each missing or unsafe file and each bad flag (no value echoed, nothing written),
//! the rerun rules, and that no other mode writes it. The helpers are `tests.rs`'s. Each test
//! says the defect it catches; the mutants planted to see them go red are in the pull request.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;
use crate::deploy::host_config::{CONFIG, HostConfig};
use crate::setup::deploy_config::DeployFlags;

const TOKEN: &str = "/etc/kbf/operator.token";
const CA: &str = "/etc/kbf/api-ca.pem";
/// What the token file holds: it must never be read, so it must never be printed.
const SECRET: &str = "operator-token-value-0123456789abcdef-never-printed";
const CA_BODY: &str =
    "-----BEGIN CERTIFICATE-----\nca-body-never-printed\n-----END CERTIFICATE-----\n";

fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

/// The flags a host that gave `installer.toml` its `[proxy]` table needs: the stage.
fn stage_only() -> DeployFlags {
    DeployFlags {
        deploy_stage: some("gamma"),
        ..DeployFlags::default()
    }
}

/// Every flag.
fn all_flags() -> DeployFlags {
    DeployFlags {
        deploy_stage: some("gamma"),
        deploy_api: some("127.0.0.1:8443"),
        deploy_api_ca: some(CA),
        deploy_server_name: some("farm.example"),
        deploy_token_file: some(TOKEN),
    }
}

fn with(mode: Mode, deploy: DeployFlags) -> Raw {
    Raw {
        deploy,
        ..raw(mode)
    }
}

/// A host with a release, the operator's token (root's, 0600) and CA (root's, 0644).
fn rig() -> FakeExec {
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, TOKEN, 0o600, ROOT, SECRET);
    put(&mut exec, CA, 0o644, ROOT, CA_BODY);
    exec
}

/// `installer.toml` with a `[proxy]` table (the `proxy_listen` tests' helper).
fn proxy_toml(listen: &str) -> String {
    format!(
        "[proxy]\n{listen}operator_token_file = \"{TOKEN}\"\nserver_cert_file = \"/etc/kbf/tls/server.pem\"\nserver_key_file = \"/etc/kbf/tls/server.key\"\nclient_ca_file = \"/etc/kbf/tls/ca.pem\"\n\n[proxy.upstream]\nbase_port = 18000\n"
    )
}

fn deploy_steps(exec: &FakeExec) -> usize {
    exec.log
        .iter()
        .filter(|s| s.to_string().contains("kbf-installer"))
        .filter(|s| !s.to_string().contains("kbf-installer.service"))
        .filter(|s| !s.to_string().contains("/var/lib/kbf-installer"))
        .count()
}

fn exists(exec: &FakeExec, path: &str) -> bool {
    exec.entries.contains_key(&hp(path))
}

const GOLDEN: &str = "{\n  \"api\": \"127.0.0.1:8443\",\n  \"api_ca\": \"/etc/kbf/api-ca.pem\",\n  \"api_server_name\": \"farm.example\",\n  \"token_file\": \"/etc/kbf/operator.token\",\n  \"stage\": \"gamma\"\n}\n";

/// Catches a key written under the wrong name, a missing key, or the wrong mode or owner: the
/// whole file of each serving mode against its bytes, root's 0644 in a root's 0755 directory,
/// written by one atomic file step (the step every file of setup is), and the secret in the
/// token file neither printed nor part of the file.
///
/// Mutants planted: `api_ca` written to `token_file`'s key; the mode 0600 (deploy, run as
/// root, is fine, but a reader of a 0644 config is the documented shape); the directory step
/// dropped.
#[test]
fn setup_writes_deploy_json_from_the_flags() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let mut exec = rig();
        let out = go_ok(&FakeOs::linux(), &mut exec, &with(mode, all_flags()));
        assert_eq!(text(&exec, CONFIG), GOLDEN, "{mode:?}");
        assert_eq!(file_meta(&exec, CONFIG), (0o644, ROOT), "{mode:?}");
        assert!(
            matches!(exec.entries.get(&hp("/etc/kbf-installer")), Some(Entry::Dir { perms, owner })
                if perms.bits() == 0o755 && *owner == ROOT),
            "{mode:?}"
        );
        assert!(
            exec.log
                .iter()
                .any(|s| matches!(s, Step::WriteFile { path, .. } if path.as_str() == CONFIG)),
            "{mode:?}"
        );
        assert!(out.contains(CONFIG), "{out}");
        assert!(out.contains("sudoers rule"), "{out}");
        assert!(!out.contains(SECRET) && !out.contains("ca-body"), "{out}");
    }
}

/// Catches a writer and a reader that drifted apart: the bytes setup wrote, put on a real file
/// and read by deploy's own `read_config`, are exactly the config setup meant, and the type
/// deploy parses them into is the one setup rendered.
///
/// Mutants planted: the reader's key `token_file` renamed; `stage` dropped from the type.
#[test]
fn deploy_reads_back_exactly_what_setup_wrote() {
    let mut exec = rig();
    go_ok(
        &FakeOs::linux(),
        &mut exec,
        &with(Mode::Frontend, all_flags()),
    );
    let (_, dir) = crate::testkit::host_root("setup-deploy-json");
    let path = dir.join("deploy.json");
    fs::write(&path, text(&exec, CONFIG)).expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode");
    let owner = fs::metadata(&path).expect("meta").uid();
    let read = crate::deploy::cli::read_config(&path, owner, true).expect("deploy reads it");
    assert_eq!(
        read,
        HostConfig {
            api: some("127.0.0.1:8443"),
            api_ca: Some(CA.into()),
            api_server_name: some("farm.example"),
            token_file: Some(TOKEN.into()),
            stage: some("gamma"),
            ..HostConfig::default()
        }
    );
    assert_eq!(read.render(), text(&exec, CONFIG));
}

/// Catches defaults that are not taken from `installer.toml`: the API is the proxy's port on
/// loopback (an unspecified or loopback listener; the packaged port with none), and the token is
/// the proxy's own `operator_token_file`, so the stage alone is enough on a host that has told
/// setup where the proxy is.
///
/// Mutants planted: the unspecified address dialled as is; the IPv6 case read as IPv4; the port
/// replaced by 8980.
#[test]
fn the_api_and_the_token_default_to_the_proxy_the_host_names() {
    let cases = [
        ("", "127.0.0.1:8980"),
        ("listen = \"0.0.0.0:8443\"\n", "127.0.0.1:8443"),
        ("listen = \"[::]:8443\"\n", "[::1]:8443"),
        ("listen = \"127.0.0.2:9000\"\n", "127.0.0.2:9000"),
        ("listen = \"[::1]:9000\"\n", "[::1]:9000"),
    ];
    for (listen, api) in cases {
        let mut exec = rig();
        put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &proxy_toml(listen));
        go_ok(
            &FakeOs::linux(),
            &mut exec,
            &with(Mode::Frontend, stage_only()),
        );
        let config = HostConfig::parse(&text(&exec, CONFIG)).expect("parses");
        assert_eq!(config.api.as_deref(), Some(api), "{listen}");
        assert_eq!(config.token_file, Some(TOKEN.into()), "{listen}");
        assert_eq!(config.api_ca, None);
    }
}

/// Catches a flag that does not win over the defaults, and a deploy aimed at an address that is
/// not this host: a proxy listening on a routable address makes setup ask for a loopback
/// `--deploy-api` (deploy refuses any other), and with the flag it goes on.
///
/// Mutants planted: the flag ignored; the non-loopback listener dialled anyway.
#[test]
fn a_routable_proxy_listener_needs_a_loopback_api_flag() {
    let mut exec = rig();
    put(
        &mut exec,
        INSTALLER_TOML,
        0o644,
        ROOT,
        &proxy_toml("listen = \"192.0.2.10:8443\"\n"),
    );
    let raw = with(Mode::Frontend, stage_only());
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    assert!(matches!(err, SetupError::Invalid(_)), "{err}");
    assert!(err.to_string().contains("--deploy-api"), "{err}");
    assert!(!err.to_string().contains("192.0.2.10"), "{err}");
    assert!(!exists(&exec, CONFIG));
    let flags = DeployFlags {
        deploy_api: some("127.0.0.1:7000"),
        ..stage_only()
    };
    go_ok(&FakeOs::linux(), &mut exec, &with(Mode::Frontend, flags));
    assert!(text(&exec, CONFIG).contains("\"api\": \"127.0.0.1:7000\""));
}

/// Catches a token that nothing names: with no `[proxy]` table and no flag there is no token to
/// point at, and setup says which flag to give rather than writing a config deploy cannot use.
///
/// Mutant planted: the missing token written as an empty path.
#[test]
fn no_token_to_name_is_refused() {
    let mut exec = rig();
    let raw = with(Mode::Frontend, stage_only());
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    assert!(err.to_string().contains("--deploy-token-file"), "{err}");
    assert_eq!(err.exit(), Exit::Usage);
    assert!(!exists(&exec, CONFIG));
}

/// Catches a referenced file that is not checked, or checked wrongly: a missing token, a link, a
/// directory, a token others can read, a token of a stranger's, a looked-at error, and a CA that
/// is missing, writable or a stranger's are each refused (exit 5) with the flag and the one command
/// that fixes it; no value (the token's text, the CA's text) is in the message, and the host is
/// not touched at all (no step ran, no file written).
///
/// Mutants planted: the mode check `!= 0o600` loosened to `& 0o077 != 0o077`; the owner check
/// dropped; the link treated as a file; the CA's write bits not looked at; the existence check
/// returning Ok.
#[test]
fn each_missing_or_unsafe_referenced_file_is_refused_with_its_fix() {
    type Break = fn(&mut FakeExec);
    let cases: [(&str, Break, &str); 10] = [
        (
            "--deploy-token-file",
            |e| {
                e.entries.remove(&hp(TOKEN));
            },
            "does not exist",
        ),
        (
            "--deploy-token-file",
            |e| {
                e.entries.insert(
                    hp(TOKEN),
                    Entry::Link {
                        target: "/x".into(),
                    },
                );
            },
            "not a regular file",
        ),
        (
            "--deploy-token-file",
            |e| {
                e.entries.insert(
                    hp(TOKEN),
                    Entry::Dir {
                        perms: Perms::new(0o700),
                        owner: ROOT,
                    },
                );
            },
            "not a regular file",
        ),
        (
            "--deploy-token-file",
            |e| put(e, TOKEN, 0o644, ROOT, SECRET),
            "chmod 600 /etc/kbf/operator.token",
        ),
        (
            "--deploy-token-file",
            |e| put(e, TOKEN, 0o640, ROOT, SECRET),
            "chmod 600 /etc/kbf/operator.token",
        ),
        (
            "--deploy-token-file",
            |e| {
                put(
                    e,
                    TOKEN,
                    0o600,
                    Owner {
                        uid: 1234,
                        gid: 1234,
                    },
                    SECRET,
                )
            },
            "chown root:root /etc/kbf/operator.token",
        ),
        (
            "--deploy-api-ca",
            |e| {
                e.entries.remove(&hp(CA));
            },
            "does not exist",
        ),
        (
            "--deploy-api-ca",
            |e| put(e, CA, 0o664, ROOT, CA_BODY),
            "chmod go-w /etc/kbf/api-ca.pem",
        ),
        (
            "--deploy-api-ca",
            |e| {
                put(
                    e,
                    CA,
                    0o644,
                    Owner {
                        uid: 1234,
                        gid: 1234,
                    },
                    CA_BODY,
                )
            },
            "chown root:root /etc/kbf/api-ca.pem",
        ),
        (
            "--deploy-api-ca",
            |e| {
                e.entries.insert(hp(CA), Entry::Other);
            },
            "not a regular file",
        ),
    ];
    for (index, (flag, spoil, fix)) in cases.iter().enumerate() {
        let mut exec = rig();
        spoil(&mut exec);
        let before = exec.entries.clone();
        let raw = with(Mode::Frontend, all_flags());
        let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
        let said = err.to_string();
        assert!(
            matches!(err, SetupError::NeedsOperator(_)),
            "{index}: {said}"
        );
        assert_eq!(err.exit(), Exit::NeedsOperator, "{index}");
        assert!(said.contains(flag), "{index}: {said}");
        assert!(said.contains(fix), "{index}: {said}");
        assert!(
            !said.contains(SECRET) && !said.contains("ca-body"),
            "{index}: {said}"
        );
        assert!(exec.log.is_empty(), "{index}: {:?}", exec.log);
        assert_eq!(exec.entries, before, "{index}");
    }
}

/// Catches a stat that fails being read as a file that is fine: a path the host cannot look at
/// is refused naming the flag, the host untouched.
///
/// Mutant planted: the error arm returning no problem.
#[test]
fn a_file_that_cannot_be_looked_at_is_refused() {
    let mut exec = rig();
    let flags = DeployFlags {
        deploy_token_file: some("/unreadable"),
        ..stage_only()
    };
    let err = go(
        &FakeOs::linux(),
        &mut exec,
        &with(Mode::Frontend, flags),
        APPLY,
    )
    .expect_err("refused");
    assert!(err.to_string().contains("cannot be looked at"), "{err}");
    assert!(exec.log.is_empty());
}

/// Catches a bad flag value that reaches the host or is echoed: each is a usage error naming
/// only the flag and the rule, and a PEM text or a token given where a path belongs is not
/// printed back.
///
/// Mutants planted: the loopback test dropped; the port-0 test dropped; the stage compared
/// case-insensitively; the PEM-text refusal dropped; a DNS label check loosened.
#[test]
fn bad_flags_are_refused_without_their_values() {
    let pem = "-----BEGIN CERTIFICATE-----\nsecret-pem-body\n-----END CERTIFICATE-----";
    let named = |name: &str| DeployFlags {
        deploy_server_name: some(name),
        ..stage_only()
    };
    let long_label = format!("{}.example", "a".repeat(64));
    let long_name = vec!["abc"; 70].join(".");
    let mut bad = vec![
        (
            DeployFlags {
                deploy_stage: some("staging"),
                ..stage_only()
            },
            "--deploy-stage: must be gamma or prod",
        ),
        (
            DeployFlags {
                deploy_api_ca: some(pem),
                ..stage_only()
            },
            "--deploy-api-ca: must be the path of a file",
        ),
        (
            DeployFlags {
                deploy_token_file: some("relative/token"),
                ..stage_only()
            },
            "--deploy-token-file: must be an absolute path",
        ),
        (
            DeployFlags {
                deploy_stage: None,
                deploy_api: some("127.0.0.1:1"),
                ..DeployFlags::default()
            },
            "--deploy-stage: required",
        ),
    ];
    for api in ["0.0.0.0:8980", "127.0.0.1:0", "farm.example:8980"] {
        let flags = DeployFlags {
            deploy_api: some(api),
            ..stage_only()
        };
        bad.push((flags, "--deploy-api: must be a loopback"));
    }
    for name in [
        "192.0.2.1",
        "a..example",
        "-a.example",
        "a-.example",
        "a_b.example",
        "",
        &long_label,
        &long_name,
    ] {
        bad.push((named(name), "--deploy-server-name: must be a DNS name"));
    }
    for (flags, rule) in bad {
        let mut exec = rig();
        let err = go(
            &FakeOs::linux(),
            &mut exec,
            &with(Mode::Frontend, flags),
            APPLY,
        )
        .expect_err("refused");
        assert!(matches!(err, SetupError::Invalid(_)), "{err}");
        assert_eq!(err.exit(), Exit::Usage);
        let said = err.to_string();
        assert!(said.contains(rule), "{rule}: {said}");
        assert!(
            !said.contains("secret-pem-body") && !said.contains("0.0.0.0"),
            "{said}"
        );
        assert!(exec.log.is_empty(), "{rule}");
    }
}

/// Catches a node mode that writes the file or silently ignores the flags: each node mode
/// refuses every `--deploy-*` flag naming it, and with no flag, as with a frontend that gave
/// none, nothing named `/etc/kbf-installer` is written.
///
/// Mutants planted: the mode gate widened to every mode; the file written whenever the host
/// serves the API, flags or not.
#[test]
fn only_a_serving_host_with_a_stage_writes_deploy_json() {
    for mode in [Mode::Daemon, Mode::Storage, Mode::StorageAndDaemon] {
        let mut exec = rig();
        let err =
            go(&FakeOs::linux(), &mut exec, &with(mode, all_flags()), APPLY).expect_err("refused");
        let said = err.to_string();
        for flag in [
            "--deploy-stage",
            "--deploy-api",
            "--deploy-api-ca",
            "--deploy-server-name",
            "--deploy-token-file",
        ] {
            assert!(
                said.contains(&format!("{flag} is for the modes that serve the API")),
                "{mode:?} {said}"
            );
        }
        assert!(exec.log.is_empty());
        let mut exec = rig();
        go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
        assert!(
            !exists(&exec, CONFIG) && !exists(&exec, "/etc/kbf-installer"),
            "{mode:?}"
        );
        assert_eq!(deploy_steps(&exec), 0, "{mode:?}");
    }
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let mut exec = rig();
        go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
        assert!(
            !exists(&exec, CONFIG) && !exists(&exec, "/etc/kbf-installer"),
            "{mode:?}"
        );
    }
}

/// Catches a dry run that writes, and a second run that writes again: without `--apply` the
/// step is printed and nothing changes; the same flags again plan no step for the file
/// (the host has what they say), and changed flags rewrite it, keeping every key setup does
/// not own and dropping the one the flags no longer give.
///
/// Mutants planted: the step planned unconditionally; the foreign keys dropped on rewrite; the
/// old `api_ca` kept when the flag is gone.
#[test]
fn a_rerun_is_idempotent_and_a_changed_flag_rewrites_keeping_other_keys() {
    let os = FakeOs::linux();
    let raw = with(Mode::Frontend, all_flags());
    let mut exec = rig();
    let dry = go(
        &os,
        &mut exec,
        &raw,
        Options {
            apply: false,
            no_start: false,
        },
    )
    .expect("dry run");
    assert!(dry.contains("DRY-RUN: ") && dry.contains(CONFIG), "{dry}");
    assert!(!exists(&exec, CONFIG));

    go_ok(&os, &mut exec, &raw);
    let first = deploy_steps(&exec);
    assert!(first >= 2, "the directory and the file");
    let out = go_ok(&os, &mut exec, &raw);
    assert_eq!(deploy_steps(&exec), first, "{out}");
    assert!(!out.contains(CONFIG), "{out}");

    // The operator's own keys, and a stage the flags change.
    put(
        &mut exec,
        CONFIG,
        0o644,
        ROOT,
        "{\"cas\": \"http://127.0.0.1:8980\", \"extra\": [1, 2], \"stage\": \"prod\", \"api_ca\": \"/old\"}",
    );
    let flags = DeployFlags {
        deploy_api_ca: None,
        ..all_flags()
    };
    let out = go_ok(&os, &mut exec, &with(Mode::Frontend, flags));
    assert!(out.contains("changed"), "{out}");
    let config = HostConfig::parse(&text(&exec, CONFIG)).expect("parses");
    assert_eq!(config.stage.as_deref(), Some("gamma"));
    assert_eq!(config.api_ca, None);
    assert_eq!(config.cas.as_deref(), Some("http://127.0.0.1:8980"));
    assert_eq!(config.other.get("extra"), Some(&serde_json::json!([1, 2])));
}

/// Catches a file that is overwritten whatever it is: one that is not a JSON object, or is not a
/// file, stops setup (exit 5) naming it and leaves it as it was, without quoting its text.
///
/// Mutants planted: the parse error ignored (default config used); the non-file case written over.
#[test]
fn a_deploy_json_that_is_not_ours_to_overwrite_is_left_alone() {
    let junk = "operator note: do not lose SECRET-NOTE";
    let mut exec = rig();
    put(&mut exec, CONFIG, 0o644, ROOT, junk);
    let raw = with(Mode::Frontend, all_flags());
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    assert!(matches!(err, SetupError::NeedsOperator(_)), "{err}");
    assert!(
        err.to_string().contains(CONFIG) && !err.to_string().contains("SECRET-NOTE"),
        "{err}"
    );
    assert_eq!(text(&exec, CONFIG), junk);

    let mut exec = rig();
    exec.entries.insert(
        hp(CONFIG),
        Entry::Dir {
            perms: Perms::new(0o755),
            owner: ROOT,
        },
    );
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    assert!(err.to_string().contains("is not a regular file"), "{err}");
    assert!(matches!(
        exec.entries.get(&hp(CONFIG)),
        Some(Entry::Dir { .. })
    ));
}

/// Catches setup writing the sudoers rule: no step of any run touches `/etc/sudoers*`, and the
/// documented rule is a held step the note points at.
///
/// Mutant planted: a step writing `/etc/sudoers.d/kbf-deploy`.
#[test]
fn setup_never_writes_sudoers() {
    let mut exec = rig();
    let out = go_ok(
        &FakeOs::linux(),
        &mut exec,
        &with(Mode::AllInOne, all_flags()),
    );
    assert!(exec.log.iter().all(|s| !s.to_string().contains("sudoers")));
    assert!(exec.entries.keys().all(|p| !p.as_str().contains("sudoers")));
    assert!(out.contains("docs/design/gamma-deploy.md"), "{out}");
}

/// Catches an owner rule that is only root: the service user's own token and CA (the operator
/// token `setup` itself makes is the service user's) are accepted, so a host that points deploy at
/// the proxy's token is not refused for the file's owner.
///
/// Mutant planted: the owner test comparing only with root.
#[test]
fn a_file_the_service_user_owns_is_accepted() {
    let mut exec = rig();
    put(&mut exec, TOKEN, 0o600, KBF_SERVER, SECRET);
    put(&mut exec, CA, 0o644, KBF_SERVER, CA_BODY);
    go_ok(
        &FakeOs::linux(),
        &mut exec,
        &with(Mode::Frontend, all_flags()),
    );
    assert_eq!(text(&exec, CONFIG), GOLDEN);
}
