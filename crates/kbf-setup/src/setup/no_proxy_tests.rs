//! Tests of `setup` built without the `proxy` feature, as the `kbf` binary links it (`kbf
//! setup`): what it refuses of the proxy's side, and that a host with no proxy is set up as it is
//! with the feature. Each test says the behaviour it asserts.

use clap::Parser;
use clap::error::ErrorKind;
use kbf_host::testkit::{FakeExec, RUNNER};
use kbf_host::{Entry, HostPath, Perms};

use super::backend::{Backend, Schemas};
use super::fake::FakeOs;
use super::kbf_settings::KbfFlags;
use super::retired::RetiredFlags;
use super::validate::Raw;
use super::{Options, SetupError, raw_from, run};
use crate::cli::{Mode, SetupArgs};
use crate::database::SchemaOps;
use crate::exit::Exit;
use crate::layout::Platform;

const SRC: &str = "/root/kbf.token";
const INSTALLER_TOML: &str = "/etc/kbf/installer.toml";
/// What a daemon host's `installer.toml` says when setup made it: no `[proxy]` table.
const DAEMON_INSTALLER_TOML: &str = "release_source = \"frontend\"
confirm_window_s = 900
confirm_window_max_s = 1800
min_generation = 0
min_protocol = 0
fetch_timeout_s = 900
max_component_bytes = 536870912
min_activation_interval_s = 60
node_cert = \"/var/lib/kbf/pki/daemon.pem\"
node_key = \"/var/lib/kbf/pki/daemon.key\"
";
/// An `installer.toml` of a host that has a proxy.
const PROXY_FILE: &str = "release_source = \"frontend\"\n\n[proxy]\nlisten = \"127.0.0.1:8443\"\n";
/// The `installer.toml` of an all-in-one host that an earlier build set up behind the proxy: a
/// `[proxy]` table and the `frontend_config` key.
const PROXY_LAYOUT: &str = include_str!("../../tests/golden/all_in_one/installer.toml");

/// A server that is never asked: the hosts here keep their state in no database.
struct NoServer;

impl Schemas for NoServer {
    fn of(&self, _: &str) -> Result<Box<dyn SchemaOps>, SetupError> {
        Err(SetupError::Database("no server".to_owned()))
    }
}

/// The command line's values for a daemon host, or for an all-in-one host with no flag.
fn raw(mode: Mode) -> Raw {
    let node = mode == Mode::Daemon;
    Raw {
        mode,
        frontend_url: node.then(|| "https://farm.example:8443".to_owned()),
        token_file: node.then(|| SRC.to_owned()),
        min_generation: None,
        retired: RetiredFlags::default(),
        database: None,
        kbf: Default::default(),
        create_local_database: false,
        installer_config: None,
    }
}

fn hp(p: &str) -> HostPath {
    HostPath::new(p).expect("a path")
}

fn put(exec: &mut FakeExec, path: &str, perms: u32, bytes: &str) {
    exec.entries.insert(
        hp(path),
        Entry::File {
            perms: Perms::new(perms),
            owner: RUNNER,
            bytes: bytes.as_bytes().to_vec(),
        },
    );
}

/// A host with the enrollment token a daemon is given.
fn host() -> FakeExec {
    let mut exec = FakeExec::new(0);
    put(&mut exec, SRC, 0o600, "tok-abc123\n");
    exec
}

/// Applies `setup` for `flags` on a Linux host, with no state database to ask.
fn go(exec: &mut FakeExec, flags: &Raw) -> Result<String, SetupError> {
    let mut out = Vec::new();
    let backend = Backend { schemas: &NoServer };
    let options = Options {
        apply: true,
        no_start: false,
    };
    run(&FakeOs::linux(), exec, &backend, flags, options, &mut out)?;
    Ok(String::from_utf8(out).expect("text"))
}

fn installer_toml(exec: &FakeExec) -> String {
    match exec.entries.get(&hp(INSTALLER_TOML)) {
        Some(Entry::File { bytes, .. }) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("{INSTALLER_TOML}: {other:?}"),
    }
}

/// The command line of `setup` as `SetupArgs` alone defines it.
#[derive(Parser)]
struct Line {
    #[command(flatten)]
    setup: SetupArgs,
}

fn parse(args: &[&str]) -> Result<Line, clap::Error> {
    Line::try_parse_from(std::iter::once("setup").chain(args.iter().copied()))
}

/// Asserts that an all-in-one host is valid in the build without the proxy, as it is with the
/// feature: it has one unit and no proxy, so the flags of its own parts are all it takes, and
/// left without `--database` it gets the local default that is provisioned.
#[test]
fn an_all_in_one_host_is_valid_without_the_proxy() {
    let some = |s: &str| Some(s.to_owned());
    let mut flags = raw(Mode::AllInOne);
    flags.kbf = KbfFlags {
        cold_store: some("file:///var/lib/kbf/cold"),
        meta_dir: some("/var/lib/kbf-meta"),
        daemon_server: some("https://farm.example:8981"),
        daemon_ca_cert: some("/etc/kbf/ca.pem"),
        daemon_cert: some("/etc/kbf/node.pem"),
        daemon_key: some("/etc/kbf/node.key"),
        node_id: some("farm-1"),
        ..KbfFlags::default()
    };
    let valid = flags.validate(Platform::Linux).expect("valid");
    assert_eq!(valid.mode, Mode::AllInOne);
    assert!(valid.database_defaulted);
    assert!(valid.local_database.is_some());
}

/// Asserts that the flags of a node are valid with no proxy, and that they give no `kbf.toml`
/// setting.
#[test]
fn a_node_is_valid_without_the_proxy() {
    let flags = raw(Mode::Daemon);
    let valid = flags.validate(Platform::Linux).expect("valid");
    assert!(valid.kbf.is_empty());
}

/// Asserts that `SetupArgs` takes none of the proxy's flags, so `kbf setup` refuses them as
/// unknown flags.
#[test]
fn the_setup_flags_refuse_the_proxy_flags() {
    for flag in ["--proxy", "--proxy-base-port=18000", "--storage-port=8990"] {
        let refused = parse(&["--mode=frontend", flag]).err();
        let kind = refused.map(|e| e.kind());
        assert_eq!(kind, Some(ErrorKind::UnknownArgument), "{flag}");
    }
}

/// Asserts that the values made from the command line carry its mode.
#[test]
fn the_values_of_the_command_line_carry_its_mode() {
    let parsed = parse(&["--mode=storage", "--no-start"]);
    let line = parsed.expect("a command line");
    let flags = raw_from(&line.setup);
    assert_eq!(flags.mode, Mode::Storage);
}

/// Asserts that a daemon host is set up with the files and steps it has with the feature: an
/// `installer.toml` with no `[proxy]` table, and nothing of the proxy in the steps or the files.
#[test]
fn a_daemon_host_is_set_up_with_nothing_of_the_proxy() {
    let mut exec = host();
    let out = go(&mut exec, &raw(Mode::Daemon)).expect("setup");
    assert_eq!(installer_toml(&exec), DAEMON_INSTALLER_TOML);
    assert!(!out.contains("proxy"), "{out}");
    let named_for_the_proxy = exec.entries.keys().any(|p| p.as_str().contains("proxy"));
    assert!(!named_for_the_proxy);
}

/// Asserts that an `installer.toml` with a `[proxy]` table is refused, that the refusal tells
/// the operator to leave the table and use `kbf-installer setup`, that only a person can go on
/// (exit 5), and that setup changed nothing.
#[test]
fn a_proxy_table_is_refused_with_the_table_left_alone() {
    let mut exec = host();
    put(&mut exec, INSTALLER_TOML, 0o644, PROXY_FILE);
    let err = go(&mut exec, &raw(Mode::Daemon)).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let said = err.to_string();
    assert!(said.contains("kbf-installer setup"), "{said}");
    assert!(said.contains("do not remove the table"), "{said}");
    assert_eq!(installer_toml(&exec), PROXY_FILE);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Asserts that an all-in-one run over the `installer.toml` of the removed proxy layout, in the
/// build without the proxy, is refused for the layout (the message names the `frontend_config`)
/// and not for its `[proxy]` table (the message does not send the operator to `kbf-installer
/// setup`), with exit 5 and nothing run on the host.
#[test]
fn the_proxy_layout_is_refused_as_the_layout_and_not_as_a_proxy_table() {
    let mut exec = host();
    put(&mut exec, INSTALLER_TOML, 0o644, PROXY_LAYOUT);
    let err = go(&mut exec, &raw(Mode::AllInOne)).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let said = err.to_string();
    assert!(said.contains("names a frontend_config"), "{said}");
    assert!(!said.contains("kbf-installer setup"), "{said}");
    assert_eq!(installer_toml(&exec), PROXY_LAYOUT);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Asserts that an `installer.toml` that is not TOML still gets the hint to fix it or remove it,
/// which the refusal of a `[proxy]` table does not give.
#[test]
fn a_file_that_is_not_toml_is_to_be_fixed_or_removed() {
    let mut exec = host();
    put(&mut exec, INSTALLER_TOML, 0o644, "= broken\n");
    let err = go(&mut exec, &raw(Mode::Daemon)).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let said = err.to_string();
    assert!(said.contains("or remove it"), "{said}");
}
