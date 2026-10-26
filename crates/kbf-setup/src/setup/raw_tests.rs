//! Tests of the values `setup` runs with, made from a command line (`raw_from`), and of the
//! proxy's flags, which `kbf-installer setup` takes only to refuse (`ProxyFlags`). Each test says
//! the behaviour it asserts.

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};

use super::proxy::ProxyFlags;
use super::raw_from;
use crate::cli::{Mode, SetupArgs};
use crate::exit::Exit;

/// The command line of `setup` as `SetupArgs` alone defines it.
#[derive(Parser)]
struct Line {
    #[command(flatten)]
    setup: SetupArgs,
}

/// The command line of `kbf-installer setup`: `SetupArgs` and the proxy's flags beside them.
#[derive(Parser)]
struct Both {
    #[command(flatten)]
    setup: SetupArgs,
    #[command(flatten)]
    proxy: ProxyFlags,
}

fn parse(args: &[&str]) -> Result<Line, clap::Error> {
    Line::try_parse_from(std::iter::once("setup").chain(args.iter().copied()))
}

fn parse_both(args: &[&str]) -> Both {
    let full = std::iter::once("setup").chain(args.iter().copied());
    Both::try_parse_from(full).expect("a command line")
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

/// Asserts that the values made from `SetupArgs` carry what the command line gave.
#[test]
fn the_values_of_the_setup_flags_carry_the_command_line() {
    let parsed = parse(&[
        "--mode=frontend",
        "--database=postgres:///kbf?host=/run/postgresql&user=kbf",
        "--min-generation=7",
    ]);
    let line = parsed.expect("a command line");
    let raw = raw_from(&line.setup);
    assert_eq!(raw.mode, Mode::Frontend);
    assert_eq!(
        raw.database.as_deref(),
        Some("postgres:///kbf?host=/run/postgresql&user=kbf")
    );
    assert_eq!(raw.min_generation, Some(7));
}

/// Asserts that `kbf-installer setup` still parses each of the proxy's flags, with the values
/// it took (a script that passes them reaches the refusal and not the parser's "unexpected
/// argument"), and that the help lists none of them.
#[test]
fn the_proxy_flags_are_parsed_and_hidden() {
    let both = parse_both(&[
        "--mode=frontend",
        "--proxy",
        "--proxy-base-port=18010",
        "--buck2-auth=bearer",
        "--buck2-token-file=/etc/kbf/bearer",
        "--storage-port=8990",
    ]);
    let want = ProxyFlags {
        proxy: true,
        proxy_base_port: Some(18_010),
        buck2_auth: Some("bearer".to_owned()),
        buck2_token_file: Some("/etc/kbf/bearer".to_owned()),
        storage_port: Some(8990),
    };
    assert_eq!(both.proxy, want);
    assert_eq!(both.setup.mode, Mode::Frontend);

    let command = Both::command();
    for id in [
        "proxy",
        "proxy_base_port",
        "buck2_auth",
        "buck2_token_file",
        "storage_port",
    ] {
        let mut args = command.get_arguments();
        let arg = args.find(|a| a.get_id().as_str() == id);
        let hidden = arg.map(clap::Arg::is_hide_set);
        assert_eq!(hidden, Some(true), "{id}");
    }
}

/// Asserts that no proxy flag means no refusal: setup goes on to its own checks.
#[test]
fn no_proxy_flag_is_no_refusal() {
    let none = ProxyFlags::default();
    assert!(none.given().is_empty());
    assert!(none.refusal().is_none());
}

/// Asserts that each of the proxy's flags, given alone, is refused as a usage error (exit 2, the
/// word `invalid_flags`), naming the flag and the reason: the two-slot supervision was removed,
/// and a proxy is now configured by hand with `[[proxy.frontend]]` tables, which the message
/// points to the design for.
#[test]
fn each_proxy_flag_is_refused_naming_it_and_the_reason() {
    type Edit = fn(&mut ProxyFlags);
    let cases: [(&str, Edit); 5] = [
        ("--proxy", |f| f.proxy = true),
        ("--proxy-base-port", |f| f.proxy_base_port = Some(18_000)),
        ("--buck2-auth", |f| f.buck2_auth = Some("none".to_owned())),
        ("--buck2-token-file", |f| {
            f.buck2_token_file = Some("/etc/kbf/bearer".to_owned())
        }),
        ("--storage-port", |f| f.storage_port = Some(8990)),
    ];
    for (flag, edit) in cases {
        let mut flags = ProxyFlags::default();
        edit(&mut flags);
        assert_eq!(flags.given(), [flag]);
        let err = flags.refusal().expect("refused");
        assert_eq!((err.exit(), err.code()), (Exit::Usage, "invalid_flags"));
        let said = err.to_string();
        assert!(said.starts_with(&format!("{flag}: ")), "{flag}: {said}");
        assert!(said.contains("two-slot supervision was removed"), "{said}");
        assert!(said.contains("[[proxy.frontend]]"), "{said}");
        assert!(said.contains("docs/design/kbf-proxy-api.md"), "{said}");
        assert!(!said.contains('\n'), "one line: {said}");
    }
}

/// Asserts that every proxy flag given together is named once, in the order of the help, in one
/// refusal.
#[test]
fn all_the_proxy_flags_together_are_named_in_one_refusal() {
    let all = ProxyFlags {
        proxy: true,
        proxy_base_port: Some(18_000),
        buck2_auth: Some("mtls".to_owned()),
        buck2_token_file: Some("/etc/kbf/bearer".to_owned()),
        storage_port: Some(8990),
    };
    let err = all.refusal().expect("refused");
    let start = "--proxy, --proxy-base-port, --buck2-auth, --buck2-token-file, --storage-port: ";
    let said = err.to_string();
    assert!(said.starts_with(start), "{said}");
    assert_eq!(said.matches("two-slot supervision was removed").count(), 1);
}
