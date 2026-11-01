//! The small parts of the ops verbs, one at a time: the parsers of what a machine and a tool
//! print, the percent-encoder of the admin alias, the ops file and the machine seam itself.

mod ops_support;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use tierstore_bench::error::BenchError;
use tierstore_bench::preflight::facts::{
    cpu_cores, link_mbps, memory_bytes, os_name, read_hardware,
};
use tierstore_bench::preflight::help::{HelpFlags, parse_help};
use tierstore_bench::preflight::host::{Cmd, Host, SystemHost, read_text, run_ok};
use tierstore_bench::preflight::live::{
    argv_of, flag_names, normalize_show, read_unit, sha256_hex, unit_absent,
};
use tierstore_bench::preflight::mc::{any_drive_healing, percent_encode, server_version};
use tierstore_bench::preflight::ops::{FILE_NAME, OpsFile, read_key_pair};
use tierstore_bench::preflight::paper::scan;

use ops_support::*;

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Catches a help parser that reads descriptions as flags, misses a short-and-long pair, reads an
/// optional usage flag as required, or reads a bare `--`: each from clap's own layout.
#[test]
fn help_flags_are_the_option_lines_and_the_required_are_the_usage_line() {
    let flags = parse_help(HELP_HOT_REQUIRED);
    assert_eq!(
        flags.all,
        set(&[
            "--listen",
            "--proxy-id",
            "--hot",
            "--cold",
            "--cold-endpoint",
            "--hot-bytes",
            "--help"
        ])
    );
    assert_eq!(
        flags.required,
        set(&["--proxy-id", "--hot", "--cold", "--hot-bytes"])
    );
    assert!(!flags.cold_only_possible());

    let optional = parse_help(
        "Usage: t [OPTIONS] [--hot <HOT>] --cold <COLD>\n\nOptions:\n  -x, --hot=<HOT>  d --nope\n      --cold <COLD>\n      --\n      -y\n",
    );
    assert_eq!(optional.all, set(&["--hot", "--cold"]));
    assert_eq!(optional.required, set(&["--cold"]));
    assert!(optional.cold_only_possible());

    assert!(
        !HelpFlags::default().cold_only_possible(),
        "no --hot at all is not cold-only"
    );
    let wanted = vec!["--cold".to_owned(), "--zzz".to_owned()];
    assert_eq!(optional.missing(&wanted), ["--zzz"]);
}

/// Catches the alias URL leaving a character raw that a URL reads as structure.
#[test]
fn percent_encoding_keeps_unreserved_characters_and_escapes_the_rest() {
    assert_eq!(percent_encode("AZaz09-._~"), "AZaz09-._~");
    assert_eq!(percent_encode("a@b:c/d e%"), "a%40b%3Ac%2Fd%20e%25");
    assert_eq!(percent_encode("é"), "%C3%A9");
}

/// Catches an ExecStart reader that keeps the start time or the pid, loses the arguments when
/// nothing follows them, or invents some where the value has none; and the flag reader taking a
/// value for a flag.
#[test]
fn exec_start_is_reduced_to_its_arguments() {
    let value = "{ path=/p ; argv[]=/p --a 1 --b=2 ; ignore_errors=no ; start_time=[x] ; pid=5 }";
    assert_eq!(argv_of(value).as_deref(), Some("/p --a 1 --b=2"));
    assert_eq!(
        argv_of("{ path=/p ; argv[]=/p --a }").as_deref(),
        Some("/p --a")
    );
    assert_eq!(argv_of("{ path=/p }"), None);
    assert_eq!(flag_names("/p --a 1 --b=2 -c x"), set(&["--a", "--b"]));

    let text = format!("Zed=1\n\nExecStart={value}\nAlpha=2\nExecStart=\nnot a property\n");
    assert_eq!(
        normalize_show(&text),
        "Alpha=2\nExecStart=\nExecStart=/p --a 1 --b=2\nZed=1"
    );
    assert_eq!(normalize_show("ExecStart={ path=/p }"), "ExecStart=");
}

/// Catches `read_unit` accepting a unit that has no command line, and the hash moving with what
/// should not move it.
#[test]
fn a_unit_without_a_command_line_is_refused_and_the_hash_ignores_the_clock() {
    let host = FakeHost::new();
    host.answer(|_| ok(&show_text(1, "a")));
    let one = read_unit(&host, "u.service").unwrap();
    host.answer(|_| ok(&show_text(1, "b")));
    assert_eq!(read_unit(&host, "u.service").unwrap(), one);
    host.answer(|_| ok(&show_text(2, "b")));
    assert_ne!(
        read_unit(&host, "u.service").unwrap().show_sha256,
        one.show_sha256
    );
    assert_eq!(
        host.ran()[0].args[..2],
        ["show".to_owned(), "u.service".to_owned()]
    );

    for text in ["Id=u\nExecStart=\n", "Id=u\n", "ExecStart={ path=/p }\n"] {
        host.answer(move |_| ok(text));
        let err = read_unit(&host, "u.service").unwrap_err();
        assert_eq!(err.to_string(), "systemctl show: the unit has no ExecStart");
    }
    host.answer(|_| failed(1, "x"));
    assert!(read_unit(&host, "u.service").is_err());
}

/// Catches `unit_absent` reading anything but `not-found` as absent.
#[test]
fn a_unit_is_absent_only_when_systemd_says_not_found() {
    let host = FakeHost::new();
    host.answer(|_| ok("not-found\n"));
    assert!(unit_absent(&host, "u").unwrap());
    host.answer(|_| ok("loaded\n"));
    assert!(!unit_absent(&host, "u").unwrap());
}

/// Catches machine facts read wrong at their edges: a CPU list with no processors, a missing or
/// odd `MemTotal`, an os-release with no name, and a link that is down or reports nothing.
#[test]
fn machine_facts_are_read_at_their_edges() {
    assert_eq!(
        cpu_cores("processor : 0\nprocessor : 1\nmodel : processor\n"),
        2
    );
    assert_eq!(cpu_cores(""), 0);
    assert_eq!(memory_bytes("MemTotal: 8 kB\n"), Some(8192));
    assert_eq!(memory_bytes("MemFree: 8 kB\n"), None);
    assert_eq!(memory_bytes("MemTotal:\n"), None);
    assert_eq!(memory_bytes("MemTotal: lots kB\n"), None);
    assert_eq!(memory_bytes(&format!("MemTotal: {} kB\n", u64::MAX)), None);
    assert_eq!(os_name("NAME=x\nPRETTY_NAME=\"Debian 12\"\n"), "Debian 12");
    assert_eq!(os_name("PRETTY_NAME=plain \n"), "plain");
    assert_eq!(os_name("NAME=x\n"), "unknown");
    assert_eq!(link_mbps("10000\n"), Some(10_000));
    assert_eq!(link_mbps("-1\n"), None);
    assert_eq!(link_mbps("0"), None);
    assert_eq!(link_mbps("fast"), None);
}

/// Catches `read_hardware` that carries on past a file it could not read, and a link speed that
/// is claimed without an interface or from one that reports none.
#[test]
fn hardware_needs_its_four_files_and_the_link_is_optional() {
    let host = healthy_host();
    let found = read_hardware(&host, None).unwrap();
    assert_eq!((found.cpu_cores, found.link_mbps), (2, None));
    assert_eq!(
        read_hardware(&host, Some("eth-mark")).unwrap().link_mbps,
        Some(10_000)
    );
    assert_eq!(read_hardware(&host, Some("other")).unwrap().link_mbps, None);
    assert_eq!(found.kernel, "6.1.0-mark");

    for path in [
        "/proc/cpuinfo",
        "/proc/meminfo",
        "/etc/os-release",
        "/proc/sys/kernel/osrelease",
    ] {
        let host = healthy_host();
        host.files.lock().unwrap().remove(Path::new(path));
        assert!(
            matches!(read_hardware(&host, None), Err(BenchError::Io { .. })),
            "{path}"
        );
    }
    let host = healthy_host();
    host.file("/proc/meminfo", "nothing");
    assert!(matches!(
        read_hardware(&host, None),
        Err(BenchError::Ops(_))
    ));
}

/// Catches the paper scan counting a line twice, missing a case, or accepting a map that is not
/// one: two lines, one marker each way, and the markers' spellings.
#[test]
fn the_paper_scan_compares_marked_lines_with_mapped_lines() {
    let page = "a\n<span class=\"tbd\">x</span> and Not Measured\nb\n[not measured]\n";
    let done = scan(page, r#"[{"paper_line": 2}, {"paper_line": 4}]"#).unwrap();
    assert!(done.agrees());
    assert_eq!(done.marked, 2);
    let off = scan(page, r#"[{"paper_line": 3}]"#).unwrap();
    assert!(!off.agrees());
    assert_eq!((off.unmapped, off.stale), (vec![2, 4], vec![3]));
    assert!(scan("", "[]").unwrap().agrees());
    for bad in ["{}", "[1]", "[{\"line\": 1}]", "nope"] {
        assert!(scan("", bad).is_err(), "{bad}");
    }
}

/// Catches the admin info readers taking the wrong server, reading a heal flag that is not `true`,
/// or failing on an answer of another shape.
#[test]
fn admin_info_is_read_for_the_version_and_the_healing_drives() {
    let info = |text: &str| serde_json::from_str::<serde_json::Value>(text).unwrap();
    let value = info(&info_json("RELEASE.x", false));
    assert_eq!(server_version(&value).as_deref(), Some("RELEASE.x"));
    assert!(!any_drive_healing(&value));
    assert!(any_drive_healing(&info(&info_json("v", true))));
    for odd in [
        "{}",
        r#"{"info": {"servers": [{"version": 1}]}}"#,
        r#"{"info": {"servers": [{"drives": [{"healing": "yes"}, {}]}, {}]}}"#,
        r#"{"info": {"servers": {}}}"#,
    ] {
        let value = info(odd);
        assert_eq!(server_version(&value), None, "{odd}");
        assert!(!any_drive_healing(&value), "{odd}");
    }
}

/// Catches an ops file that is lenient about what it is given: a field it does not know, a missing
/// section and a key file with an extra field are each refused; and a role that is not a role.
#[test]
fn the_ops_file_and_key_files_refuse_what_they_do_not_know() {
    let host = FakeHost::new();
    let mut bad = ops_json();
    bad["surprise"] = serde_json::json!(1);
    host.file(&format!("/d/{FILE_NAME}"), &bad.to_string());
    let err = OpsFile::load(&host, Path::new("/d")).unwrap_err();
    assert!(
        err.to_string().contains("unknown field `surprise`"),
        "{err}"
    );

    host.file(&format!("/e/{FILE_NAME}"), &ops_json().to_string());
    let ops = OpsFile::load(&host, Path::new("/e")).unwrap();
    assert_eq!(ops.tierstore.version, "0.0.37");

    host.file(
        "/k.json",
        r#"{"access_key_id": "a", "secret_access_key": "b"}"#,
    );
    assert_eq!(
        read_key_pair(&host, Path::new("/k.json"))
            .unwrap()
            .access_key_id,
        "a"
    );
    host.file(
        "/k2.json",
        r#"{"access_key_id": "a", "secret_access_key": "TOPSECRET", "x": 1}"#,
    );
    let err = read_key_pair(&host, Path::new("/k2.json")).unwrap_err();
    assert!(!err.to_string().contains("TOPSECRET"));
    assert!(read_key_pair(&host, Path::new("/none.json")).is_err());
    assert_eq!(
        format!(
            "{:?}",
            read_key_pair(&host, Path::new("/k.json"))
                .unwrap()
                .secret_access_key
        ),
        "<redacted>"
    );
}

/// Catches `Cmd` printing what it carries, and `run_ok` putting output or arguments into an error:
/// the error holds the status only, and a command that cannot start says so by kind.
#[test]
fn a_command_never_prints_its_arguments_and_its_error_never_holds_output() {
    let cmd =
        Cmd::new("tool", &["--secret", "SECRET-MARK"]).with_env("E", "SECRET-MARK".to_owned());
    let shown = format!("{cmd:?}");
    assert!(!shown.contains("SECRET"), "{shown}");
    assert!(shown.contains("tool") && shown.contains("2 args") && shown.contains("1 variables"));

    let host = FakeHost::new();
    host.answer(|_| failed(3, "boom SECRET-MARK"));
    let err = run_ok(&host, "tool", &cmd).unwrap_err();
    assert_eq!(err.to_string(), "tool: exited Some(3)");
    host.answer(|_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
    let err = run_ok(&host, "tool", &cmd).unwrap_err();
    assert_eq!(err.to_string(), "tool: cannot start: NotFound");
    host.answer(|_| ok("out"));
    assert_eq!(run_ok(&host, "tool", &cmd).unwrap(), "out");
    host.answer(|_| {
        Ok(tierstore_bench::preflight::host::CmdOutput {
            success: false,
            code: None,
            stdout: String::new(),
            stderr: "x".repeat(500),
        })
    });
    assert_eq!(
        run_ok(&host, "tool", &cmd).unwrap_err().to_string(),
        "tool: exited None"
    );
}

/// A TCP socket bound to a loopback port that nothing listens on, and the port.
fn bound_not_listening() -> (rustix::fd::OwnedFd, u16) {
    use rustix::net::{AddressFamily, SocketAddrV4, SocketType, bind, getsockname, socket};
    let fd = socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
    bind(&fd, &SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = SocketAddrV4::try_from(getsockname(&fd).unwrap())
        .unwrap()
        .port();
    (fd, port)
}

/// Catches the real machine seam answering differently from what its callers read: a file read and
/// a file that is not UTF-8, a path that exists or not, a command that exits 0, one that exits
/// non-zero, one that sees its variable, one that is not there, a port with a listener and one
/// without, and a load reading (a result either way: the reader is the gate's).
#[test]
fn the_system_host_reads_files_runs_commands_and_tries_ports() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-host");
    let file = dir.path().join("f");
    std::fs::write(&file, b"a\xffb").unwrap();
    let host = SystemHost;
    assert_eq!(host.read_file(&file).unwrap(), b"a\xffb");
    assert_eq!(read_text(&host, &file).unwrap(), "a\u{fffd}b");
    assert!(host.exists(&file));
    assert!(!host.exists(&dir.path().join("none")));
    assert!(matches!(
        read_text(&host, &dir.path().join("none")),
        Err(BenchError::Io { .. })
    ));

    let said = host.run(&Cmd::new("echo", &["hello"])).unwrap();
    assert!(said.success);
    assert_eq!((said.code, said.stdout.as_str()), (Some(0), "hello\n"));
    let refused = host.run(&Cmd::new("false", &[])).unwrap();
    assert!(!refused.success);
    assert_eq!(refused.code, Some(1));
    let with = host
        .run(&Cmd::new("env", &[]).with_env("TB_HOST_TEST", "set-by-test".to_owned()))
        .unwrap();
    assert!(with.stdout.contains("TB_HOST_TEST=set-by-test"));
    let echoed = host
        .run(&Cmd::new("cat", &[]).with_stdin("line one\nline two\n".to_owned()))
        .unwrap();
    assert_eq!(echoed.stdout, "line one\nline two\n");
    let nothing = host.run(&Cmd::new("cat", &[])).unwrap();
    assert_eq!(nothing.stdout, "", "no input is a closed input, not a hang");
    let deaf = host
        .run(&Cmd::new("false", &[]).with_stdin("x".repeat(1 << 20)))
        .unwrap();
    assert!(
        !deaf.success,
        "a child that never reads is not a failed write"
    );
    let shown = format!(
        "{:?}",
        Cmd::new("cat", &[]).with_stdin("TOPSECRET".to_owned())
    );
    assert!(
        shown.contains("with input") && !shown.contains("TOPSECRET"),
        "{shown}"
    );
    assert!(format!("{:?}", Cmd::new("cat", &[])).contains("no input"));
    let err = host
        .run(&Cmd::new("tierstore-bench-no-such-program", &[]))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(host.port_open(port));
    // A port bound and not listening is held (no other test can be given it) and refuses
    // connections; a port that was freed could be handed to another test binary's listener.
    let held = bound_not_listening();
    assert!(!host.port_open(held.1));
    drop((held, listener));

    let _reading = host.load_sample();
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// Catches a command that cannot be stopped, or a stop that kills more than the command: with the
/// flag down a command runs to its end as `run` does (output, input, status, a start that
/// fails, one that takes a moment); with the flag up, one that would not end by itself is
/// killed and the answer is `None`.
#[test]
fn the_system_host_stops_a_command_it_is_told_to_stop() {
    let host = SystemHost;
    let down = AtomicBool::new(false);
    let said = host
        .run_until(&Cmd::new("echo", &["hello"]), &down)
        .unwrap()
        .unwrap();
    assert!(said.success);
    assert_eq!((said.code, said.stdout.as_str()), (Some(0), "hello\n"));
    let echoed = host
        .run_until(
            &Cmd::new("cat", &[]).with_stdin("line one\n".to_owned()),
            &down,
        )
        .unwrap()
        .unwrap();
    assert_eq!(echoed.stdout, "line one\n");
    let refused = host
        .run_until(&Cmd::new("false", &[]), &down)
        .unwrap()
        .unwrap();
    assert_eq!((refused.success, refused.code), (false, Some(1)));
    let slow = host
        .run_until(&Cmd::new("sleep", &["0.1"]), &down)
        .unwrap()
        .unwrap();
    assert!(slow.success);
    assert!(
        host.run_until(&Cmd::new("/nonexistent-tb/none", &[]), &down)
            .is_err()
    );

    let up = AtomicBool::new(true);
    assert_eq!(
        host.run_until(&Cmd::new("sleep", &["3600"]), &up).unwrap(),
        None
    );
}
