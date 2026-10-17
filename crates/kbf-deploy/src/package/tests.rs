//! The `package` command line: its flags, its exit codes, and that `kbf-deploy package`
//! reaches the verbs.

use std::ffi::OsString;

use clap::Parser;
use kbf_host::testkit::FakeExec;

use super::*;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    verb: Package,
}

fn parse(args: &[&str]) -> Result<Package, clap::Error> {
    let args = std::iter::once("package").chain(args.iter().copied());
    Cli::try_parse_from(args).map(|c| c.verb)
}

/// Catches `--purge-data` accepted without `--purge` (a remove that deletes the data), a
/// flag a verb should not take, and dry run being anything but the default.
#[test]
fn the_flags_of_each_verb() {
    let Package::Postinst(a) = parse(&["postinst"]).expect("parses") else {
        panic!("postinst");
    };
    assert_eq!(a.mode(), Mode::DryRun);
    let Package::Postinst(a) = parse(&["postinst", "--apply"]).expect("parses") else {
        panic!("postinst");
    };
    assert_eq!(a.mode(), Mode::Apply);
    let Package::Prerm(a) = parse(&["prerm", "--upgrade"]).expect("parses") else {
        panic!("prerm");
    };
    assert!(a.upgrade && !a.apply.apply);
    let Package::Postrm(a) = parse(&["postrm", "--purge", "--purge-data", "--apply"]).expect("ok")
    else {
        panic!("postrm");
    };
    assert!(a.purge && a.purge_data && a.apply.apply);
    for bad in [
        &["postrm", "--purge-data"][..],
        &["postinst", "--root", "/x"],
        &["postinst", "--purge"],
        &["prerm", "--purge"],
        &["postrm", "--upgrade"],
        &["install"],
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}

fn kbf_deploy(args: &[&str]) -> (ExitCode, String, String) {
    let argv: Vec<OsString> = std::iter::once("kbf-deploy")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::cli::run(argv, &mut FakeExec::new(0), &|_| None, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).expect("utf-8"),
        String::from_utf8(err).expect("utf-8"),
    )
}

/// Catches the kit missing from the command line, a refusal that is not exit 2 with a
/// `REFUSED:` line (the host here has no setuid `newuidmap`), and the `prerm` and `postrm`
/// dry runs reaching their verbs.
#[test]
fn kbf_deploy_package_reaches_the_verbs() {
    let (code, out, err) = kbf_deploy(&["package", "postinst"]);
    assert_eq!(code, ExitCode::from(2), "{out}{err}");
    assert!(
        err.starts_with("REFUSED: newuidmap is missing or does not run (no /usr/bin/newuidmap)"),
        "{err}"
    );
    let (code, out, err) = kbf_deploy(&["package", "prerm", "--upgrade"]);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert!(
        out.contains("upgrade: kbf-node.service keeps running"),
        "{out}"
    );
    let (code, out, err) = kbf_deploy(&["package", "postrm"]);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert!(out.contains("nothing to do"), "{out}");
    // A host whose `getent` answers nothing readable: the purge is refused, not guessed.
    let (code, _, err) = kbf_deploy(&["package", "postrm", "--purge"]);
    assert_eq!(code, ExitCode::from(2));
    assert!(err.starts_with("REFUSED: "), "{err}");
    let (code, _, err) = kbf_deploy(&["package", "postrm", "--purge-data"]);
    assert_eq!(code, ExitCode::from(2));
    assert!(err.contains("--purge"), "{err}");
}

/// Catches a failed step reported as a refusal (it ran: exit 1, `kbf-deploy:` line) and a
/// refusal reported as a failure, through the command line's own writer.
#[test]
fn a_failed_step_prints_without_refused() {
    let mut exec = FakeExec::new(0);
    exec.fail_at = Some(0);
    exec.entries.insert(
        HostPath::new("/run/systemd/system").expect("path"),
        kbf_host::Entry::Dir {
            perms: kbf_host::step::Perms::new(0o755),
            owner: kbf_host::testkit::RUNNER,
        },
    );
    exec.answer = Box::new(|cmd| {
        Ok(kbf_host::testkit::reply(
            i32::from(cmd.args.first().map(String::as_str) == Some("is-active")) ^ 1,
            "",
            "",
        ))
    });
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let verb = Package::Prerm(PrermArgs {
        upgrade: false,
        apply: ApplyArgs { apply: true },
    });
    let code = run(verb, &mut exec, &Env::host(), &mut out, &mut err);
    let err = String::from_utf8(err).expect("utf-8");
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(
        err.starts_with("kbf-deploy: step 0 (run /usr/bin/systemctl stop kbf-node.service) failed"),
        "{err}"
    );
}

/// Catches the exit code of each error kind: a step that failed (or a copy that did not
/// match) is 1, and everything else a refusal, 2.
#[test]
fn exit_codes() {
    let step = HostError::Step {
        index: 0,
        step: "s".to_owned(),
        source: io::Error::other("x"),
    };
    assert_eq!(PackageError::Host(step).exit(), 1);
    let out = HostError::Output(io::Error::other("x"));
    assert_eq!(PackageError::Host(out).exit(), 1);
    let step = HostError::Step {
        index: 0,
        step: "s".to_owned(),
        source: io::Error::other("x"),
    };
    assert_eq!(
        PackageError::Linux(crate::linux::HostError::Deploy(step)).exit(),
        1
    );
    let copy = PackageError::Copy {
        path: path("/x"),
        want: "a".to_owned(),
        got: "b".to_owned(),
    };
    assert_eq!(copy.exit(), 1);
    assert_eq!(PackageError::RootUser.exit(), 2);
    assert_eq!(PackageError::Host(HostError::NotRoot { euid: 1 }).exit(), 2);
    assert_eq!(
        PackageError::Linux(crate::linux::HostError::NoUser("kbf".to_owned())).exit(),
        2
    );
}

/// Catches an output error (a closed pipe) being lost: `say` and a phase's heading
/// return it.
#[test]
fn a_closed_output_is_an_error() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let e = say(&mut Closed, "x").expect_err("closed");
    assert!(matches!(e, PackageError::Output(_)));
    assert_eq!(e.exit(), 2);
}
