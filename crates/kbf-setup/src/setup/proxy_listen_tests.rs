//! Tests of the `kbf-proxy.socket` drop-in `setup` writes from `[proxy] listen` of
//! `installer.toml`. The helpers are `tests.rs`'s. Each test says the defect it catches.

use super::*;
use crate::setup::paths::{PROXY_LISTEN_DROPIN, PROXY_SOCKET_DIR};

const UNITS: &[&str] = &["kbf-frontend.service"];

/// The operator's `[proxy]` table, with `extra` on top of it.
fn table(extra: &str) -> String {
    format!(
        "[proxy]\n{extra}operator_token_file = \"/etc/kbf/operator.token\"\nserver_cert_file = \"/etc/kbf/tls/server.pem\"\nserver_key_file = \"/etc/kbf/tls/server.key\"\nclient_ca_file = \"/etc/kbf/tls/ca.pem\"\n\n[proxy.upstream]\nbase_port = 18000\n"
    )
}

/// A frontend host whose `installer.toml` is `body`.
fn with_toml(body: &str) -> FakeExec {
    let mut exec = host();
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, body);
    exec
}

/// The steps that touched the socket's drop-in, as printed.
fn dropin_steps(exec: &FakeExec) -> Vec<String> {
    exec.log
        .iter()
        .map(ToString::to_string)
        .filter(|s| s.contains("kbf-proxy.socket.d"))
        .collect()
}

fn frontend(os: &FakeOs, exec: &mut FakeExec) -> String {
    go_ok(os, exec, &raw(Mode::Frontend))
}

const WANT: &str = "[Socket]\nListenStream=\nListenStream=192.0.2.10:8443\n";

/// Catches the drop-in being wrong in any byte (without the empty `ListenStream=` the socket
/// would listen on 8980 and the new address), not root's 0644, in a directory that is not
/// root's 0755, or setup reaching for systemd (a reload or a restart of the public port) when
/// it was not asked to: the output must tell the operator both are needed.
///
/// Catches: the empty `ListenStream=` line left out; the mode 0600; a
/// `systemctl restart kbf-proxy.socket` step added; the note left out.
#[test]
fn a_listen_address_becomes_the_exact_dropin_and_nothing_is_restarted() {
    let mut exec = with_toml(&table("listen = \"192.0.2.10:8443\"\n"));
    let out = frontend(&FakeOs::linux(), &mut exec);
    assert_eq!(text(&exec, PROXY_LISTEN_DROPIN), WANT);
    assert_eq!(file_meta(&exec, PROXY_LISTEN_DROPIN), (0o644, ROOT));
    assert!(matches!(
        exec.entries.get(&hp(PROXY_SOCKET_DIR)),
        Some(Entry::Dir { perms, owner }) if perms.bits() == 0o755 && *owner == ROOT
    ));
    for cmd in cmds(&exec) {
        assert!(
            !cmd.contains("daemon-reload") && !cmd.contains("restart") && !cmd.contains("proxy"),
            "{cmd}"
        );
    }
    assert!(
        out.contains("note:")
            && out.contains("systemctl daemon-reload")
            && out.contains("systemctl restart kbf-proxy.socket"),
        "{out}"
    );
}

/// Catches an IPv6 address written without its brackets (systemd would read the last colon
/// as the port separator wrongly) and the address not going through the socket type.
#[test]
fn an_ipv6_address_keeps_its_brackets() {
    let mut exec = with_toml(&table("listen = \"[2001:db8::1]:8980\"\n"));
    frontend(&FakeOs::linux(), &mut exec);
    assert_eq!(
        text(&exec, PROXY_LISTEN_DROPIN),
        "[Socket]\nListenStream=\nListenStream=[2001:db8::1]:8980\n"
    );
}

/// Catches a drop-in written when the operator set no address, and one left behind from a
/// run that had it: no `listen` writes nothing and removes what an earlier run wrote, and
/// the output says a reload is due.
///
/// Catches: the removal step left out; a drop-in written with the default port.
#[test]
fn no_listen_writes_no_dropin_and_removes_an_old_one() {
    let mut exec = with_toml(&table(""));
    let out = frontend(&FakeOs::linux(), &mut exec);
    assert!(dropin_steps(&exec).is_empty(), "{:?}", dropin_steps(&exec));
    assert!(!exec.entries.contains_key(&hp(PROXY_LISTEN_DROPIN)));
    assert!(!out.contains("daemon-reload"), "{out}");

    let mut exec = with_toml(&table(""));
    exec.entries.insert(
        hp(PROXY_SOCKET_DIR),
        Entry::Dir {
            perms: Perms::new(0o755),
            owner: ROOT,
        },
    );
    put(&mut exec, PROXY_LISTEN_DROPIN, 0o644, ROOT, WANT);
    let out = frontend(&FakeOs::linux(), &mut exec);
    assert!(!exec.entries.contains_key(&hp(PROXY_LISTEN_DROPIN)));
    assert!(out.contains("systemctl daemon-reload"), "{out}");
}

/// Catches a re-run that rewrites an unchanged drop-in (a pointless reload note every run),
/// one that keeps the old address after the key changed, and one that loses the drop-in when
/// the key is removed. Also catches setup erasing the operator's `[proxy]` table while it
/// rewrites `installer.toml` (the next run would then remove the drop-in).
///
/// Catches: `proxy` left out of `InstallerToml::render`; the file compared by its
/// length only.
#[test]
fn a_rerun_changes_the_dropin_only_when_the_key_changed() {
    let mut exec = with_toml(&table("listen = \"192.0.2.10:8443\"\n"));
    frontend(&FakeOs::linux(), &mut exec);
    let again = settled(UNITS);
    // The first run rewrote installer.toml: the table is still there, with its address.
    assert!(text(&exec, INSTALLER_TOML).contains("listen = \"192.0.2.10:8443\""));
    let steps = exec.log.len();
    let out = frontend(&again, &mut exec);
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
    assert!(!out.contains("daemon-reload"), "{out}");

    let moved = text(&exec, INSTALLER_TOML).replace("192.0.2.10:8443", "192.0.2.20:9443");
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &moved);
    let out = frontend(&again, &mut exec);
    assert_eq!(
        text(&exec, PROXY_LISTEN_DROPIN),
        "[Socket]\nListenStream=\nListenStream=192.0.2.20:9443\n"
    );
    assert!(out.contains("systemctl daemon-reload"), "{out}");

    let removed = text(&exec, INSTALLER_TOML).replace("listen = \"192.0.2.20:9443\"\n", "");
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &removed);
    let out = frontend(&again, &mut exec);
    assert!(!exec.entries.contains_key(&hp(PROXY_LISTEN_DROPIN)));
    assert!(out.contains("systemctl daemon-reload"), "{out}");
}

/// Catches a bad address reaching the drop-in, and the contract's strictness slipping:
/// every bad `listen` stops setup with exit 5 before any step, naming the key and never
/// echoing the value; a key `[proxy]` does not know is refused too.
///
/// Catches: the port-0 check removed; the error built from the value.
#[test]
fn an_invalid_listen_is_refused_naming_the_key() {
    for bad in [
        "\"farm.example:8980\"",
        "\"192.0.2.10\"",
        "\"192.0.2.10:0\"",
        "\"2001:db8::1:8980\"",
        "8980",
    ] {
        let body = table(&format!("listen = {bad}\n"));
        let mut exec = with_toml(&body);
        let err =
            go(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend), APPLY).expect_err("refused");
        assert_eq!(err.exit(), Exit::NeedsOperator, "{bad}");
        let why = err.to_string();
        assert!(why.contains("listen"), "{why}");
        assert!(!why.contains("8980"), "{why}");
        assert!(
            !why.contains("192.0.2.10") && !why.contains("farm.example"),
            "{why}"
        );
        assert!(exec.log.is_empty(), "{bad}: {:?}", exec.log);
    }
    let mut exec = with_toml(&table("listn = \"192.0.2.10:8443\"\n"));
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend), APPLY).expect_err("unknown");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(exec.log.is_empty());
}

/// Catches the drop-in being planned on a Mac, which has no systemd (setup there would fail
/// writing under /etc/systemd).
#[test]
fn a_mac_gets_no_dropin() {
    let mut exec = provisioned(with_toml(&table("listen = \"192.0.2.10:8443\"\n")));
    go_ok(&FakeOs::mac(), &mut exec, &raw(Mode::Daemon));
    assert!(dropin_steps(&exec).is_empty());
}
