//! Tests of the values `setup` runs with, made from a command line: `raw_from` (what `kbf
//! setup` runs with) and `raw_with_proxy` (what `kbf-installer setup` runs with). Each test says
//! the behaviour it asserts.

use clap::Parser;
use clap::error::ErrorKind;

use super::proxy::ProxyFlags;
use super::{raw_from, raw_with_proxy};
use crate::cli::{Mode, SetupArgs};

/// The command line of `setup` as `SetupArgs` alone defines it.
#[derive(Parser)]
struct Line {
    #[command(flatten)]
    setup: SetupArgs,
}

fn parse(args: &[&str]) -> Result<Line, clap::Error> {
    Line::try_parse_from(std::iter::once("setup").chain(args.iter().copied()))
}

fn proxy_flags() -> ProxyFlags {
    ProxyFlags {
        proxy: true,
        proxy_base_port: Some(18_010),
        ..Default::default()
    }
}

/// Asserts that `SetupArgs` takes none of the proxy's flags, so a program that parses only it
/// (`kbf setup`) refuses them as unknown flags, whatever features the library is built with.
#[test]
fn the_setup_flags_alone_refuse_the_proxy_flags() {
    for flag in ["--proxy", "--proxy-base-port=18000", "--storage-port=8990"] {
        let refused = parse(&["--mode=frontend", flag]).err();
        let kind = refused.map(|e| e.kind());
        assert_eq!(kind, Some(ErrorKind::UnknownArgument), "{flag}");
    }
}

/// Asserts that the values made from `SetupArgs` alone are those of a host that is not behind
/// the proxy: no proxy flag is set.
#[test]
fn the_values_of_the_setup_flags_alone_have_no_proxy_flags() {
    let parsed = parse(&["--mode=storage", "--no-start"]);
    let line = parsed.expect("a command line");
    let raw = raw_from(&line.setup);
    assert_eq!(raw.mode, Mode::Storage);
    assert_eq!(raw.proxy, ProxyFlags::default());
    assert!(!raw.behind_proxy());
}

/// Asserts that the values made with the proxy's flags carry them, and carry every other value
/// the command line gave just as `raw_from` does.
#[test]
fn the_values_with_the_proxy_flags_carry_them_beside_the_setup_flags() {
    let parsed = parse(&[
        "--mode=frontend",
        "--database=postgres:///kbf?host=/run/postgresql&user=kbf",
        "--min-generation=7",
    ]);
    let line = parsed.expect("a command line");
    let flags = proxy_flags();
    let plain = raw_from(&line.setup);
    let raw = raw_with_proxy(&line.setup, &flags);
    assert_eq!(raw.proxy, flags);
    assert!(raw.behind_proxy());
    assert_eq!(raw.mode, plain.mode);
    assert_eq!(raw.database, plain.database);
    assert_eq!(raw.min_generation, Some(7));
}
