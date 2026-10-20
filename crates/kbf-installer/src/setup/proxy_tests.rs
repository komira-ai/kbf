//! Tests of `setup --proxy` (B-g): the `[proxy]` table written into `installer.toml`, the
//! units enabled instead of `kbf-frontend.service`, what a rerun does with a table that is
//! there, and a refusal for each flag that would write a value nothing reads. The helpers
//! are `tests.rs`'s. Each test says the defect it catches; the mutants planted to see them go
//! red are in the pull request.

use super::*;
use crate::installer_toml::InstallerToml;
use crate::setup::kbf_settings::KbfFlags;
use crate::setup::proxy::{AuthFlag, DEFAULT_BASE_PORT, ProxyFlags, ProxyPlan, leftover_tls_keys};

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

/// Catches a proxy host that still enables `kbf-frontend.service` (two things would then
/// want the same ports and the same state), or one that leaves the proxy's units out or in the
/// wrong order (the socket holds the port before the service takes it): exactly the socket,
/// the service, the backup timer and the installer's own, the first two enabled and not started.
#[test]
fn a_proxy_host_enables_the_proxy_units_and_not_the_frontend_instance() {
    let mut exec = proxy_host();
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(
        systemctl(&exec),
        [
            "run /usr/bin/systemctl enable -- kbf-proxy.socket",
            "run /usr/bin/systemctl enable -- kbf-proxy.service",
            "run /usr/bin/systemctl enable --now -- kbf-backup.timer",
            "run /usr/bin/systemctl enable --now -- kbf-installer.service",
        ]
    );
}

/// Catches a host without `--proxy` changing: its `installer.toml` has no `[proxy]` table, its
/// unit is `kbf-frontend.service`, and the proxy's units are not touched.
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
            "run /usr/bin/systemctl enable -- kbf-frontend.service",
            "run /usr/bin/systemctl enable --now -- kbf-backup.timer",
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
    assert!(!out.contains("kbf-frontend.service"), "{out}");
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches a rerun that does work, or rewrites the table: the same flags on a finished host
/// run no step.
#[test]
fn a_rerun_of_a_proxy_host_is_a_no_op() {
    let mut exec = proxy_host();
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    let steps = exec.log.len();
    let units = ["kbf-proxy.socket", "kbf-proxy.service", "kbf-backup.timer"];
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

/// A proxy host's file whose certificates are a `[[proxy.server_cert]]` list (the default
/// entry is the certificate the flags name, a second one is chosen by name) instead of the
/// single pair, spelled as `render` writes it.
fn sni_list() -> String {
    let single =
        "server_cert_file = \"/etc/kbf/reapi.pem\"\nserver_key_file = \"/etc/kbf/reapi.key\"\n";
    assert!(PROXY_GOLDEN.contains(single));
    PROXY_GOLDEN.replace(single, "").replace(
        "\n[proxy.upstream]",
        "\n[[proxy.server_cert]]\nnames = []\ncert_file = \"/etc/kbf/reapi.pem\"\nkey_file = \"/etc/kbf/reapi.key\"\ndefault = true\n\n[[proxy.server_cert]]\nnames = [\"a.example\"]\ncert_file = \"/etc/kbf/a.pem\"\nkey_file = \"/etc/kbf/a.key\"\n\n[proxy.upstream]",
    )
}

/// Catches `setup` dropping or rewriting the proxy's `[[server_cert]]` list on a rerun (the
/// flags name the pair, the table holds it as the default entry): the table is one the
/// proxy's own parser accepts with both entries, the file is left byte for byte, no write of
/// it is planned.
#[test]
fn a_rerun_passes_a_server_cert_list_through_unchanged() {
    let mut exec = proxy_host();
    let there = sni_list();
    let parsed = InstallerToml::parse(&there).expect("the proxy's parser takes the list");
    assert_eq!(parsed.proxy.as_ref().map(|p| p.server_cert.len()), Some(2));
    assert_eq!(parsed.render(), there);
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &there);
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(text(&exec, INSTALLER_TOML), there);
    assert!(
        !exec
            .log
            .iter()
            .any(|s| matches!(s, Step::WriteFile { path, .. } if path.as_str() == INSTALLER_TOML)),
        "the file was rewritten: {:?}",
        exec.log
    );
}

/// Catches a rewrite over a list: flags that differ from the list's default entry, or from
/// another owned key, are refused naming the key and no value, and the file stays as it was.
/// The list is the operator's, so the table counts as tuned.
#[test]
fn a_differing_owned_key_over_a_server_cert_list_is_refused() {
    let there = sni_list();
    type RawEdit = fn(&mut Raw);
    let edits: [(&str, RawEdit); 2] = [
        ("server_cert_file", |r| {
            r.kbf.reapi_tls_cert = some("/etc/kbf/other.pem")
        }),
        ("upstream.base_port", |r| {
            r.proxy.proxy_base_port = Some(19_000)
        }),
    ];
    for (key, edit) in edits {
        let mut exec = proxy_host();
        put(&mut exec, "/etc/kbf/other.pem", 0o644, ROOT, "cert");
        put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &there);
        let mut raw = proxy_raw();
        edit(&mut raw);
        let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err(key);
        assert_eq!(err.exit(), Exit::NeedsOperator, "{key}");
        let shown = err.to_string();
        assert!(shown.contains(&format!("({key})")), "{key}: {shown}");
        assert!(
            !shown.contains("other.pem") && !shown.contains("19000"),
            "{shown}"
        );
        assert_eq!(text(&exec, INSTALLER_TOML), there, "{key}");
    }
}

/// Catches `--proxy` on a Mac, where the proxy's systemd units do not exist: refused by
/// validation, naming the flag.
#[test]
fn the_proxy_is_for_linux() {
    let found = proxy_raw()
        .validate(Platform::MacOs)
        .expect_err("refused")
        .join("; ");
    assert!(found.contains("--proxy is for Linux"), "{found}");
}

/// One row per key `setup` owns: the key as the refusal names it, and the golden file with
/// only that key different from what `proxy_raw`'s flags say.
fn owned_rows() -> [(&'static str, String); 7] {
    let golden = |from: &str, to: &str| {
        assert!(PROXY_GOLDEN.contains(from), "{from}");
        PROXY_GOLDEN.replacen(from, to, 1)
    };
    [
        (
            "operator_token_file",
            golden(
                "operator_token_file = \"/etc/kbf/api-token\"",
                "operator_token_file = \"/etc/kbf/zz-token\"",
            ),
        ),
        (
            "server_cert_file",
            golden("/etc/kbf/reapi.pem", "/etc/kbf/zz.pem"),
        ),
        (
            "server_key_file",
            golden("/etc/kbf/reapi.key", "/etc/kbf/zz.key"),
        ),
        (
            "client_ca_file",
            golden(
                "client_ca_file = \"/etc/kbf/ca.pem\"",
                "client_ca_file = \"/etc/kbf/zz-ca.pem\"",
            ),
        ),
        (
            "buck2_auth",
            golden("buck2_auth = \"none\"", "buck2_auth = \"mtls\""),
        ),
        (
            "buck2_token_file",
            golden(
                "buck2_auth = \"none\"\n",
                "buck2_auth = \"none\"\nbuck2_token_file = \"/etc/kbf/zz-bearer\"\n",
            ),
        ),
        (
            "upstream.base_port",
            golden("base_port = 18000", "base_port = 19000"),
        ),
    ]
}

/// Catches a Keep that ignores a differing owned key (`setup` then says nothing and the proxy
/// runs on the old value), for every key `setup` owns and not only the port: with other
/// settings in the table (`hold_max_s`), the one key differing from the flags refuses
/// (exit 5), names that key and no other, never a value, and writes nothing. A comparison
/// dropped or mislabelled in `differing` fails its row.
#[test]
fn a_differing_key_in_a_tuned_table_is_refused_not_overwritten() {
    for (key, golden) in owned_rows() {
        let mut exec = proxy_host();
        let there = golden.replace("hold_max_s = 20", "hold_max_s = 33");
        put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &there);
        let err = go(&FakeOs::linux(), &mut exec, &proxy_raw(), APPLY).expect_err(key);
        assert_eq!(err.exit(), Exit::NeedsOperator, "{key}");
        let shown = err.to_string();
        assert!(shown.contains(&format!("({key})")), "{key}: {shown}");
        for value in ["19000", "18000", "zz-", "zz.", "mtls"] {
            assert!(!shown.contains(value), "{key}: {shown}");
        }
        assert_eq!(text(&exec, INSTALLER_TOML), there, "{key}");
        assert!(exec.log.is_empty(), "{key}: {:?}", exec.log);
    }
}

/// Catches a refusal where there is nothing to lose, and a rewrite that leaves an owned key
/// behind: for every key `setup` owns, a table that differs from the flags in that key only
/// and is the defaults otherwise is rewritten to the golden file (so `take_owned` must copy
/// every key it owns, or the row refuses instead).
#[test]
fn a_differing_key_in_an_untuned_table_is_rewritten() {
    for (key, there) in owned_rows() {
        let mut exec = proxy_host();
        put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &there);
        go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
        assert_eq!(text(&exec, INSTALLER_TOML), PROXY_GOLDEN, "{key}");
    }
}

/// Catches a Keep that is not one: for every owned key, the same flags over a tuned table that
/// holds the flags' value leave the file byte for byte, so no key of `differing` can report a
/// difference where there is none.
#[test]
fn an_owned_key_equal_to_its_flag_is_kept() {
    let mut exec = proxy_host();
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &tuned());
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(text(&exec, INSTALLER_TOML), tuned());
}

/// Catches a rerun that deletes what a newer `[proxy]` table holds and `setup` has no flag
/// for (the `[[proxy.server_cert]]` list of SNI): the file is refused or kept with the list,
/// never rewritten without it.
#[test]
fn a_proxy_table_key_setup_does_not_know_is_not_dropped() {
    let there = format!(
        "{PROXY_GOLDEN}\n[[proxy.server_cert]]\nnames = [\"reapi.example.com\"]\ncert_file = \"/etc/kbf/reapi.pem\"\nkey_file = \"/etc/kbf/reapi.key\"\ndefault = true\n"
    );
    let mut exec = proxy_host();
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &there);
    let kept = match go(&FakeOs::linux(), &mut exec, &proxy_raw(), APPLY) {
        Err(_) => text(&exec, INSTALLER_TOML) == there,
        Ok(_) => text(&exec, INSTALLER_TOML).contains("server_cert"),
    };
    assert!(kept, "{}", text(&exec, INSTALLER_TOML));
}

/// Catches a converted host whose `kbf.toml` still has the TLS keys of the host before the
/// proxy: its children would serve TLS behind a proxy that speaks h2c to them. `setup` refuses
/// (exit 5) naming each key and not its path, and writes nothing; without `--proxy` the same
/// file is accepted as before.
#[test]
fn a_kbf_toml_with_the_children_tls_keys_is_refused_on_a_proxy_host() {
    let old = FRONTEND_TOML.replace(
        "api-token-file = \"/etc/kbf/api-token\"\n",
        "api-token-file = \"/etc/kbf/api-token\"\nreapi-tls-cert = \"/etc/kbf/reapi.pem\"\nreapi-tls-key = \"/etc/kbf/reapi.key\"\nworker-client-ca = \"/etc/kbf/ca.pem\"\nworker-tls-cert = \"/etc/kbf/w.pem\"\nworker-tls-key = \"/etc/kbf/w.key\"\n",
    );
    assert_ne!(old, FRONTEND_TOML);
    let mut exec = proxy_host();
    put(&mut exec, KBF_TOML, 0o640, ROOT, &old);
    let err = go(&FakeOs::linux(), &mut exec, &proxy_raw(), APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let shown = err.to_string();
    assert!(
        shown.contains(
            "(reapi-tls-cert, reapi-tls-key, worker-client-ca, worker-tls-cert, worker-tls-key)"
        ),
        "{shown}"
    );
    assert!(!shown.contains("/etc/kbf/reapi"), "{shown}");
    assert_eq!(text(&exec, KBF_TOML), old);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches a table written beside a frontend unit that is still enabled: setup does not stop
/// or disable it, and says so.
#[test]
fn an_enabled_frontend_instance_is_named_and_left_alone() {
    let mut exec = proxy_host();
    let mut os = FakeOs::linux();
    os.enabled.insert("kbf-frontend.service".to_owned());
    let out = go_ok(&os, &mut exec, &proxy_raw());
    assert!(
        out.contains("kbf-frontend.service is enabled and the proxy starts the frontend itself"),
        "{out}"
    );
    assert!(
        systemctl(&exec)
            .iter()
            .all(|c| !c.contains("disable") && !c.contains("kbf-frontend.service")),
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
        let found = raw
            .validate(Platform::Linux)
            .expect_err("refused")
            .join("; ");
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
/// slots fit) and a range over the ports a default host uses (the public socket's 8980, the
/// operator API's 8982 and the storage node's 8990): the edges on both sides, for slot A and
/// for slot B.
#[test]
fn the_base_port_is_in_range_and_clear_of_the_default_ports() {
    let check = |port: u16| {
        let mut raw = proxy_raw();
        raw.proxy.proxy_base_port = Some(port);
        raw.validate(Platform::Linux)
            .map(|_| ())
            .map_err(|p| p.join("; "))
    };
    for ok in [1024, 8965, 8975, 8983, 8985, 8991, 18_000, 65_521] {
        assert!(check(ok).is_ok(), "{ok}: {:?}", check(ok));
    }
    for (bad, rule) in [
        (1023, "must be 1024 to 65521"),
        (65_522, "must be 1024 to 65521"),
        (8966, "would cover"),
        (8970, "would cover"),
        (8976, "would cover"),
        (8982, "would cover"),
        (8986, "would cover"),
        (8990, "would cover"),
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
        raw.validate(Platform::Linux)
            .map(|v| v.proxy)
            .map_err(|p| p.join("; "))
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

/// Catches `--buck2-auth mtls` lost on the way to the file (the third spelling of the flag).
#[test]
fn mtls_reaches_the_table() {
    let mut raw = proxy_raw();
    raw.proxy.buck2_auth = Some(AuthFlag::Mtls);
    let mut exec = proxy_host();
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    let file = text(&exec, INSTALLER_TOML);
    assert!(file.contains("buck2_auth = \"mtls\"\n"), "{file}");
}

/// Catches a `kbf.toml` that does not parse, or has no `[tls]` table, counted as holding
/// the children's TLS keys: neither has any (the `kbf.toml` check refuses the first by
/// itself), while a table with one names it.
#[test]
fn leftover_tls_keys_are_only_the_ones_in_a_tls_table() {
    assert!(leftover_tls_keys("not [ toml").is_empty());
    assert!(leftover_tls_keys("mode = \"frontend\"\n").is_empty());
    assert_eq!(
        leftover_tls_keys("[tls]\nworker-tls-key = \"/k\"\n"),
        ["worker-tls-key"]
    );
}

/// Catches `merge` trusting a plan the proxy would refuse: a plan built by hand with bearer
/// and no key file is an error naming the flag, not a table.
#[test]
fn a_plan_the_proxy_would_refuse_is_not_merged() {
    let mut raw = proxy_raw();
    raw.proxy.buck2_auth = Some(AuthFlag::Bearer);
    raw.proxy.buck2_token_file = some("/etc/kbf/bearer");
    let mut plan: ProxyPlan = raw
        .validate(Platform::Linux)
        .expect("valid")
        .proxy
        .expect("a plan");
    plan.buck2_token_file = None;
    let err = plan.merge(None).expect_err("refused");
    assert!(err.to_string().contains("--proxy"), "{err}");
}

/// Catches a list of one certificate that is not marked `default` being taken for a different
/// certificate than the flags' pair: it is the default (the only one), so a rerun with its
/// files keeps the file byte for byte.
#[test]
fn the_only_entry_of_a_server_cert_list_is_the_default() {
    let one = sni_list();
    let a = one
        .find("\n[[proxy.server_cert]]\nnames = [\"a.example\"]")
        .expect("second entry");
    let end = one.find("\n[proxy.upstream]").expect("upstream");
    let there = format!("{}{}", &one[..a], &one[end..])
        .replace("names = []\n", "names = [\"b.example\"]\n")
        .replace("default = true\n", "");
    let mut exec = proxy_host();
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &there);
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(text(&exec, INSTALLER_TOML), there);
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
