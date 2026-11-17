//! Tests of `setup` built without the `proxy` feature, as the `kbf` binary links it (`kbf
//! setup`): what it refuses of the proxy's side, and that a host with no proxy is set up as it is
//! with the feature. Each test says the behaviour it asserts.

use clap::Parser;
use clap::error::ErrorKind;
use kbf_host::testkit::{FakeExec, RUNNER};
use kbf_host::{Entry, HostPath, Perms};

use super::backend::{Backend, Schemas};
use super::fake::FakeOs;
use super::retired::RetiredFlags;
use super::validate::Raw;
use super::{Options, SetupError, raw_from, run};
use crate::cli::{Mode, SetupArgs};
use crate::database::SchemaOps;
use crate::exit::Exit;
use crate::layout::Platform;

const SRC: &str = "/root/kbf.token";
const INSTALLER_TOML: &str = "/etc/kbf/installer.toml";
/// What a storage host's `installer.toml` says when setup made it: no `[proxy]` table.
const STORAGE_INSTALLER_TOML: &str = "release_source = \"frontend\"
confirm_window_s = 900
confirm_window_max_s = 1800
min_generation = 0
min_protocol = 0
fetch_timeout_s = 900
max_component_bytes = 536870912
min_activation_interval_s = 60
node_cert = \"/var/lib/kbf/pki/storage.pem\"
node_key = \"/var/lib/kbf/pki/storage.key\"
";
/// An `installer.toml` of a host that has a proxy.
const PROXY_FILE: &str = "release_source = \"frontend\"\n\n[proxy]\nlisten = \"127.0.0.1:8443\"\n";

/// A server that is never asked: the hosts here keep their state in no database.
struct NoServer;

impl Schemas for NoServer {
    fn of(&self, _: &str) -> Result<Box<dyn SchemaOps>, SetupError> {
        Err(SetupError::Database("no server".to_owned()))
    }
}

/// The command line's values for a storage host.
fn raw(mode: Mode) -> Raw {
    let node = mode == Mode::Storage;
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

/// A host with the enrollment token a storage node is given.
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

/// Asserts that the flags of a node are valid with no proxy, and that they give no `kbf.toml`
/// setting.
#[test]
fn a_node_is_valid_without_the_proxy() {
    let flags = raw(Mode::Storage);
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

/// Asserts that a storage host is set up with the files and steps it has with the feature: an
/// `installer.toml` with no `[proxy]` table, and nothing of the proxy in the steps or the files.
#[test]
fn a_storage_host_is_set_up_with_nothing_of_the_proxy() {
    let mut exec = host();
    let out = go(&mut exec, &raw(Mode::Storage)).expect("setup");
    assert_eq!(installer_toml(&exec), STORAGE_INSTALLER_TOML);
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
    let err = go(&mut exec, &raw(Mode::Storage)).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let said = err.to_string();
    assert!(said.contains("kbf-installer setup"), "{said}");
    assert!(said.contains("do not remove the table"), "{said}");
    assert_eq!(installer_toml(&exec), PROXY_FILE);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Asserts that an `installer.toml` that is not TOML still gets the hint to fix it or remove it,
/// which the refusal of a `[proxy]` table does not give.
#[test]
fn a_file_that_is_not_toml_is_to_be_fixed_or_removed() {
    let mut exec = host();
    put(&mut exec, INSTALLER_TOML, 0o644, "= broken\n");
    let err = go(&mut exec, &raw(Mode::Storage)).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let said = err.to_string();
    assert!(said.contains("or remove it"), "{said}");
}
