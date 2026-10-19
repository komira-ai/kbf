//! Tests of `setup --proxy` (B-g): the `[proxy]` table written into `installer.toml`, the
//! units enabled instead of `kbf-frontend@a.service`, what a rerun does with a table that is
//! there, and a refusal for each flag that would write a value nothing reads. The helpers
//! are `tests.rs`'s. Each test says the defect it catches; the mutants planted to see them go
//! red are in the pull request.

use super::*;
use crate::installer_toml::InstallerToml;
use crate::setup::kbf_settings::KbfFlags;
use crate::setup::proxy::{AuthFlag, DEFAULT_BASE_PORT, ProxyFlags};

/// The `installer.toml` of a proxy host with every flag at its default: a file that decodes
/// with `InstallerToml::parse` and renders back to itself.
const PROXY_GOLDEN: &str = include_str!("../../tests/golden/proxy_host/installer.toml");

/// An edit of the `kbf.toml` flags, and of the proxy's, for the tables of cases.
type Edit = fn(&mut KbfFlags);
type ProxyEdit = fn(&mut ProxyFlags);

fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

/// The flags of a proxy host: the proxy's certificate pair and client CA, and the storage node.
fn proxy_raw() -> Raw {
    Raw {
        kbf: KbfFlags {
            storage_url: some("http://127.0.0.1:8990"),
            reapi_tls_cert: some("/etc/kbf/reapi.pem"),
            reapi_tls_key: some("/etc/kbf/reapi.key"),
            worker_client_ca: some("/etc/kbf/ca.pem"),
            ..KbfFlags::default()
        },
        proxy: ProxyFlags {
            proxy: true,
            ..ProxyFlags::default()
        },
        ..raw(Mode::Frontend)
    }
}

/// A host with a release and the files `proxy_raw` names (`ca.pem` is `host()`'s).
fn proxy_host() -> FakeExec {
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, "/etc/kbf/reapi.key", 0o600, KBF_SERVER, "key");
    put(&mut exec, "/etc/kbf/reapi.pem", 0o644, ROOT, "cert");
    exec
}

fn systemctl(exec: &FakeExec) -> Vec<String> {
    cmds(exec)
        .into_iter()
        .filter(|c| c.contains("systemctl"))
        .collect()
}

/// Catches a proxy host that is not written as the proxy reads it: the whole `installer.toml`
/// byte for byte (the flags' keys in `[proxy]`, `[proxy.upstream] base_port` at the default,
/// the operator token setup makes, every default spelled out), root-owned 0644, and a
/// `kbf.toml` with none of the proxy's TLS keys: the children speak plain h2c behind it.
#[test]
fn a_proxy_host_gets_the_proxy_table_and_a_kbf_toml_without_tls_files() {
    let mut exec = proxy_host();
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(text(&exec, INSTALLER_TOML), PROXY_GOLDEN);
    assert_eq!(file_meta(&exec, INSTALLER_TOML), (0o644, ROOT));
    assert_eq!(text(&exec, KBF_TOML), FRONTEND_TOML);
    let parsed = InstallerToml::parse(PROXY_GOLDEN).expect("the golden parses");
    let table = parsed.proxy.expect("a proxy table");
    assert_eq!(table.upstream.base_port, DEFAULT_BASE_PORT);
    assert_eq!(table.operator_token_file.to_str(), Some(API_TOKEN));
}

/// Catches a proxy host that still enables `kbf-frontend@a.service` (two things would then
/// want the same ports and the same state), or one that leaves the proxy's units out or in the
/// wrong order (the socket holds the port before the service takes it): exactly the socket,
/// the service and the installer's own, the first two enabled and not started.
#[test]
fn a_proxy_host_enables_the_proxy_units_and_not_the_frontend_instance() {
    let mut exec = proxy_host();
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(
        systemctl(&exec),
        [
            "run /usr/bin/systemctl enable -- kbf-proxy.socket",
            "run /usr/bin/systemctl enable -- kbf-proxy.service",
            "run /usr/bin/systemctl enable --now -- kbf-installer.service",
        ]
    );
}

/// Catches a host without `--proxy` changing: its `installer.toml` has no `[proxy]` table, its
/// unit is `kbf-frontend@a.service`, and the proxy's units are not touched.
#[test]
fn a_frontend_host_without_the_flag_is_what_it_was() {
    let mut exec = proxy_host();
    let plain = Raw {
        proxy: ProxyFlags::default(),
        ..raw(Mode::Frontend)
    };
    go_ok(&FakeOs::linux(), &mut exec, &plain);
    assert!(!text(&exec, INSTALLER_TOML).contains("proxy"));
    assert_eq!(
        systemctl(&exec),
        [
            "run /usr/bin/systemctl enable -- kbf-frontend@a.service",
            "run /usr/bin/systemctl enable --now -- kbf-installer.service",
        ]
    );
}

/// Catches a dry run that writes, or one that does not show the table: every step is a
/// `DRY-RUN:` line, nothing is applied, and the proxy's units are in the plan.
#[test]
fn a_dry_run_shows_the_proxy_plan_and_changes_nothing() {
    let mut exec = proxy_host();
    let dry = Options {
        apply: false,
        no_start: false,
    };
    let out = go(&FakeOs::linux(), &mut exec, &proxy_raw(), dry).expect("dry run");
    assert!(
        out.contains("DRY-RUN: write /etc/kbf/installer.toml"),
        "{out}"
    );
    assert!(
        out.contains("DRY-RUN: run /usr/bin/systemctl enable -- kbf-proxy.socket"),
        "{out}"
    );
    assert!(!out.contains("kbf-frontend@a"), "{out}");
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches a rerun that does work, or rewrites the table: the same flags on a finished host
/// run no step.
#[test]
fn a_rerun_of_a_proxy_host_is_a_no_op() {
    let mut exec = proxy_host();
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    let steps = exec.log.len();
    let units = ["kbf-proxy.socket", "kbf-proxy.service"];
    go_ok(&settled(&units), &mut exec, &proxy_raw());
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
}

/// A proxy host's file with `hold_max_s` tuned by hand, a key setup has no flag for.
fn tuned() -> String {
    PROXY_GOLDEN.replace("hold_max_s = 20", "hold_max_s = 33")
}

/// Catches `render` dropping the table, which made `setup` delete a hand-written `[proxy]`
/// on a rerun (its doc said a rewrite kept it): a frontend run without `--proxy` over a file
/// that has the table leaves the file byte for byte, tells the operator, and enables the
/// frontend unit as before.
#[test]
fn a_rerun_without_the_flag_keeps_a_proxy_table_that_is_there() {
    let mut exec = proxy_host();
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &tuned());
    let plain = Raw {
        proxy: ProxyFlags::default(),
        ..raw(Mode::Frontend)
    };
    let out = go_ok(&FakeOs::linux(), &mut exec, &plain);
    assert_eq!(text(&exec, INSTALLER_TOML), tuned());
    assert!(
        out.contains("a [proxy] table and --proxy was not given"),
        "{out}"
    );
}

/// Catches a rerun that wipes what the operator tuned in the table: with `--proxy` and the
/// owned keys already as the flags say, the file is kept byte for byte, `hold_max_s` and all.
#[test]
fn a_proxy_rerun_keeps_the_keys_it_has_no_flag_for() {
    let mut exec = proxy_host();
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &tuned());
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(text(&exec, INSTALLER_TOML), tuned());
    assert!(
        !exec
            .log
            .iter()
            .any(|s| matches!(s, Step::WriteFile { path, .. } if path.as_str() == INSTALLER_TOML)),
        "the file was rewritten: {:?}",
        exec.log
    );
}

/// Catches a Keep that ignores a differing owned key (`setup` then says nothing and the proxy
/// binds the old ports): with other settings in the table, a different `--proxy-base-port`
/// refuses (exit 5), names the key and not the value, and writes nothing.
#[test]
fn a_differing_key_in_a_tuned_table_is_refused_not_overwritten() {
    let mut exec = proxy_host();
    let there = tuned().replace("base_port = 18000", "base_port = 19000");
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &there);
    let err = go(&FakeOs::linux(), &mut exec, &proxy_raw(), APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let shown = err.to_string();
    assert!(shown.contains("upstream.base_port"), "{shown}");
    assert!(
        !shown.contains("19000") && !shown.contains("18000"),
        "{shown}"
    );
    assert_eq!(text(&exec, INSTALLER_TOML), there);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches a refusal where there is nothing to lose: a table that differs from the flags in
/// the keys setup owns and is the defaults otherwise is rewritten to say what the flags say.
#[test]
fn a_differing_key_in_an_untuned_table_is_rewritten() {
    let mut exec = proxy_host();
    let there = PROXY_GOLDEN.replace("base_port = 18000", "base_port = 19000");
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &there);
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(text(&exec, INSTALLER_TOML), PROXY_GOLDEN);
}

/// Catches a table written beside a frontend unit that is still enabled: setup does not stop
/// or disable it, and says so.
#[test]
fn an_enabled_frontend_instance_is_named_and_left_alone() {
    let mut exec = proxy_host();
    let mut os = FakeOs::linux();
    os.enabled.insert("kbf-frontend@a.service".to_owned());
    let out = go_ok(&os, &mut exec, &proxy_raw());
    assert!(
        out.contains("kbf-frontend@a.service is enabled and the proxy starts the frontend itself"),
        "{out}"
    );
    assert!(
        systemctl(&exec)
            .iter()
            .all(|c| !c.contains("disable") && !c.contains("frontend@a")),
        "{:?}",
        systemctl(&exec)
    );
}

/// The flags of a refusal: `proxy_raw` after `edit`; nothing may be read or written.
fn refused(edit: impl FnOnce(&mut Raw)) -> String {
    let mut raw = proxy_raw();
    edit(&mut raw);
    let mut exec = proxy_host();
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::Usage, "{err}");
    assert!(
        exec.log.is_empty() && exec.queries.borrow().is_empty(),
        "{:?}",
        exec.log
    );
    let shown = format!("{err} {err:?}");
    for value in ["/etc/kbf/reapi", "/etc/kbf/ca.pem", "/etc/kbf/bearer"] {
        assert!(!shown.contains(value), "{shown}");
    }
    shown
}

/// Catches a flag that would write a value nothing reads, or is silently dropped: on a proxy
/// host the listener flags and the worker listener's certificate pair are refused by name
/// (the proxy gives the children their loopback ports and they serve plain h2c).
#[test]
fn flags_the_proxy_makes_pointless_are_refused() {
    let edits: [(&str, Edit); 5] = [
        ("--listen", |k| k.listen = some("0.0.0.0:8980")),
        ("--worker-listen", |k| {
            k.worker_listen = some("0.0.0.0:8981")
        }),
        ("--api-listen", |k| k.api_listen = some("127.0.0.1:8982")),
        ("--worker-tls-cert", |k| {
            k.worker_tls_cert = some("/etc/kbf/worker.pem")
        }),
        ("--worker-tls-key", |k| {
            k.worker_tls_key = some("/etc/kbf/worker.key")
        }),
    ];
    for (flag, edit) in edits {
        let shown = refused(|r| edit(&mut r.kbf));
        assert!(
            shown.contains(&format!("{flag} is not taken with --proxy")),
            "{flag}: {shown}"
        );
    }
}

/// Catches `--proxy` accepted where the proxy cannot serve (it starts `kbf --mode=frontend`
/// children), and its helper flags accepted without it.
#[test]
fn the_proxy_flags_belong_to_a_frontend_and_to_each_other() {
    for mode in [
        Mode::AllInOne,
        Mode::Daemon,
        Mode::Storage,
        Mode::StorageAndDaemon,
    ] {
        let raw = Raw {
            proxy: proxy_raw().proxy,
            ..raw(mode)
        };
        let mut exec = proxy_host();
        let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
        assert!(
            err.to_string().contains("--proxy is for --mode=frontend"),
            "{mode:?}: {err}"
        );
        assert!(exec.log.is_empty(), "{mode:?}");
    }
    let without: [(&str, ProxyEdit); 3] = [
        ("--proxy-base-port", |p| p.proxy_base_port = Some(18_000)),
        ("--buck2-auth", |p| p.buck2_auth = Some(AuthFlag::None)),
        ("--buck2-token-file", |p| {
            p.buck2_token_file = some("/etc/kbf/bearer")
        }),
    ];
    for (flag, edit) in without {
        let mut raw = Raw {
            proxy: ProxyFlags::default(),
            ..raw(Mode::Frontend)
        };
        edit(&mut raw.proxy);
        let found = raw.validate().expect_err("refused").join("; ");
        assert!(found.contains(&format!("{flag} is for --proxy")), "{found}");
    }
}

/// Catches a proxy table missing a file the proxy cannot start without: each of the three
/// is required by name, and a certificate without its key still fails.
#[test]
fn the_proxy_needs_its_certificate_key_and_client_ca() {
    let edits: [(&str, Edit); 3] = [
        ("--reapi-tls-cert", |k| k.reapi_tls_cert = None),
        ("--reapi-tls-key", |k| k.reapi_tls_key = None),
        ("--worker-client-ca", |k| k.worker_client_ca = None),
    ];
    for (flag, edit) in edits {
        let shown = refused(|r| edit(&mut r.kbf));
        assert!(
            shown.contains(&format!("{flag}: required with --proxy")),
            "{flag}: {shown}"
        );
    }
}

/// Catches `--proxy-base-port` limits that drift from the proxy's (1024 to 65521, so both
/// slots fit) and a range over the ports a default host uses (the public socket's 8980 and
/// the operator API's 8982): the edges on both sides, for slot A and for slot B.
#[test]
fn the_base_port_is_in_range_and_clear_of_the_default_ports() {
    let check = |port: u16| {
        let mut raw = proxy_raw();
        raw.proxy.proxy_base_port = Some(port);
        raw.validate().map(|_| ()).map_err(|p| p.join("; "))
    };
    for ok in [1024, 8965, 8983, 18_000, 65_521] {
        assert!(check(ok).is_ok(), "{ok}: {:?}", check(ok));
    }
    for (bad, rule) in [
        (1023, "must be 1024 to 65521"),
        (65_522, "must be 1024 to 65521"),
        (8966, "would cover"),
        (8970, "would cover"),
        (8976, "would cover"),
        (8982, "would cover"),
    ] {
        let found = check(bad).expect_err("refused");
        assert!(found.contains(rule), "{bad}: {found}");
    }
}

/// Catches `--buck2-auth bearer` accepted without a key file (the proxy would refuse to
/// start), a key file beside another policy (a value nothing reads), and one that is not a
/// path.
#[test]
fn bearer_needs_its_key_file_and_only_bearer_takes_one() {
    let found = |auth, file: Option<&str>| {
        let mut raw = proxy_raw();
        raw.proxy.buck2_auth = auth;
        raw.proxy.buck2_token_file = file.map(str::to_owned);
        raw.validate().map(|v| v.proxy).map_err(|p| p.join("; "))
    };
    assert!(
        found(Some(AuthFlag::Bearer), None)
            .expect_err("refused")
            .contains("bearer"),
    );
    assert!(
        found(Some(AuthFlag::None), Some("/etc/kbf/bearer"))
            .expect_err("refused")
            .contains("--buck2-token-file is for --buck2-auth bearer")
    );
    assert!(
        found(Some(AuthFlag::Bearer), Some("bearer"))
            .expect_err("refused")
            .contains("--buck2-token-file")
    );
    let plan = found(Some(AuthFlag::Bearer), Some("/etc/kbf/bearer"))
        .expect("valid")
        .expect("a proxy plan");
    assert_eq!(plan.buck2_token_file.as_deref(), Some("/etc/kbf/bearer"));
}

/// Catches the bearer policy and its key file lost on the way to the file: both keys are in
/// the table, and the file decodes with the proxy's own parser.
#[test]
fn the_buck2_policy_reaches_the_table() {
    let mut raw = proxy_raw();
    raw.proxy.buck2_auth = Some(AuthFlag::Bearer);
    raw.proxy.buck2_token_file = some("/etc/kbf/bearer");
    raw.proxy.proxy_base_port = Some(19_000);
    let mut exec = proxy_host();
    put(&mut exec, "/etc/kbf/bearer", 0o600, KBF_SERVER, "tok");
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    let file = text(&exec, INSTALLER_TOML);
    assert!(file.contains("buck2_auth = \"bearer\"\n"), "{file}");
    assert!(
        file.contains("buck2_token_file = \"/etc/kbf/bearer\"\n"),
        "{file}"
    );
    let table = InstallerToml::parse(&file)
        .expect("parses")
        .proxy
        .expect("table");
    assert_eq!(table.upstream.base_port, 19_000);
}

/// Catches the proxy's files going unchecked: a key the service user does not own, or a
/// certificate that is not there, stops setup before it writes (as for the `kbf.toml` files),
/// naming the flag and no path.
#[test]
fn the_proxy_files_are_checked_like_the_children_files() {
    let mut exec = proxy_host();
    put(&mut exec, "/etc/kbf/reapi.key", 0o600, ROOT, "key");
    exec.entries.remove(&hp("/etc/kbf/ca.pem"));
    let err = go(&FakeOs::linux(), &mut exec, &proxy_raw(), APPLY).expect_err("refused");
    let shown = err.to_string();
    assert!(
        shown.contains("--reapi-tls-key: a key must be owned"),
        "{shown}"
    );
    assert!(
        shown.contains("--worker-client-ca: no such file"),
        "{shown}"
    );
    assert!(!shown.contains("/etc/kbf"), "{shown}");
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}
