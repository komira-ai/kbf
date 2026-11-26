//! `cargo xtask farm-config` as a process: the masking lines it prints first, the refusals (exit
//! status 1, naming the flag), the usage errors (exit status 2) and the file it writes. The same
//! checks on the library's functions are in `src/farm_config/tests.rs`.

use std::path::Path;
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

// A host name: only the refusals and the usage errors use it, which end before the verb resolves
// the host, so no test asks the system resolver for a name.
const ADDRESS: &str = "grpc://farm.example:8980";
const PROPERTIES: &str = "OSFamily=Linux,ISA=x86-64";

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-farm-config-cli"),
        name,
    )
}

/// `xtask farm-config` with these values and `--out`.
fn farm_config(address: &str, instance: &str, tls: &str, properties: &str, out: &Path) -> Output {
    Command::new(XTASK)
        .args(["farm-config", "--address", address, "--instance", instance])
        .args(["--tls", tls, "--properties", properties, "--out"])
        .arg(out)
        .output()
        .expect("run xtask")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn the_file_is_written_whole_in_a_directory_made_for_it_and_only_its_name_is_printed() {
    // Catches: a config that differs from what buck2 and buildrules read, a missing parent
    // directory failing the verb, and the file's contents (the worker properties, the address)
    // printed to the public log beside its name.
    let dir = scratch("whole");
    let out = dir
        .join("home")
        .join(".buckconfig.d")
        .join("farm.buckconfig");
    // An IP literal: the verb's resolver turns it into itself with no lookup, so the test needs
    // no DNS; the resolved forms of a host name are the unit tests' (src/farm_config/tests.rs).
    let o = farm_config("grpc://192.0.2.5:8980", "main", "true", PROPERTIES, &out);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        format!(
            "::add-mask::grpc://192.0.2.5:8980\n::add-mask::192.0.2.5:8980\n\
             ::add-mask::192.0.2.5\nfarm-config: wrote {}\n",
            out.display()
        )
    );
    assert_eq!(stderr(&o), "");
    assert_eq!(
        std::fs::read_to_string(&out).expect("read"),
        "\
[buck2_re_client]
  engine_address = grpc://192.0.2.5:8980
  action_cache_address = grpc://192.0.2.5:8980
  cas_address = grpc://192.0.2.5:8980
  instance_name = main
  tls = true
  execution_concurrency_limit = 256
  grpc_keepalive_timeout_secs = 60
  max_concurrent_uploads_per_action = 4
[buildrules_re]
  linux_x86_64_properties = OSFamily=Linux,ISA=x86-64
"
    );
}

#[test]
fn an_address_without_a_scheme_is_masked_once_and_tls_false_is_written() {
    // Catches: a double mask for an address that has no scheme (the address, then its bare host,
    // each once: an IP resolves to itself, already masked), and `tls = false` not written.
    let dir = scratch("plain");
    let out = dir.join("farm.buckconfig");
    let o = farm_config("192.0.2.3:8980", "main", "false", PROPERTIES, &out);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        format!(
            "::add-mask::192.0.2.3:8980\n::add-mask::192.0.2.3\nfarm-config: wrote {}\n",
            out.display()
        )
    );
    let text = std::fs::read_to_string(&out).expect("read");
    assert!(
        text.contains("  engine_address = 192.0.2.3:8980\n"),
        "{text}"
    );
    assert!(text.contains("  tls = false\n"), "{text}");
}

#[test]
fn every_refusal_exits_1_names_its_flag_prints_the_masks_first_and_writes_nothing() {
    // Catches: a refusal that exits 0 or 2, one that does not name its flag, a refusal that
    // comes before the masking lines (so the log shows the address), and a file written anyway.
    let dir = scratch("refusals");
    let out = dir.join("farm.buckconfig");
    let blank = "is empty: a fork's run gets no variables; refusing, because buck2 would build on \
                 the runner";
    let cases = [
        (
            ["", "main", "true", PROPERTIES],
            format!("--address {blank}"),
            "",
        ),
        (
            [ADDRESS, " ", "true", PROPERTIES],
            format!("--instance {blank}"),
            "::add-mask::grpc://farm.example:8980\n::add-mask::farm.example:8980\n",
        ),
        (
            [ADDRESS, "main", "true", ""],
            format!("--properties {blank}"),
            "::add-mask::grpc://farm.example:8980\n::add-mask::farm.example:8980\n",
        ),
        (
            [ADDRESS, "main", "yes", PROPERTIES],
            "--tls is \"yes\": it must be exactly `true` or `false`".to_owned(),
            "::add-mask::grpc://farm.example:8980\n::add-mask::farm.example:8980\n",
        ),
        (
            ["farm.example", "main", "true", PROPERTIES],
            "--address is not host:port, optionally behind a scheme such as grpc://: it has no \
             `:port`"
                .to_owned(),
            "::add-mask::farm.example\n",
        ),
        (
            ["farm.example:http", "main", "true", PROPERTIES],
            "--address is not host:port, optionally behind a scheme such as grpc://: the port is \
             not a number from 0 to 65535"
                .to_owned(),
            "::add-mask::farm.example:http\n",
        ),
        (
            [ADDRESS, "ma\nin", "true", PROPERTIES],
            "--instance holds a line break: each value is one line of the buckconfig".to_owned(),
            "::add-mask::grpc://farm.example:8980\n::add-mask::farm.example:8980\n",
        ),
        (
            [ADDRESS, "main", "true", "a=b\rc=d"],
            "--properties holds a line break: each value is one line of the buckconfig".to_owned(),
            "::add-mask::grpc://farm.example:8980\n::add-mask::farm.example:8980\n",
        ),
    ];
    for ([address, instance, tls, properties], words, printed) in cases {
        let o = farm_config(address, instance, tls, properties, &out);
        assert_eq!(o.status.code(), Some(1), "{words}");
        assert_eq!(stdout(&o), printed, "{words}");
        assert_eq!(stderr(&o), format!("xtask: {words}\n"));
        assert!(!out.exists(), "{words}");
    }
}

#[test]
fn a_file_that_cannot_be_written_exits_1_naming_it() {
    // Catches: a config that could not be written reported as written.
    let dir = scratch("unwritable");
    let o = farm_config("192.0.2.5:8980", "main", "true", PROPERTIES, dir.path());
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        format!(
            "xtask: cannot write {:?}: Is a directory (os error 21)\n",
            dir.path()
        )
    );
}

#[test]
fn a_missing_or_unknown_flag_is_a_usage_error_with_status_2_and_prints_no_mask() {
    // Catches: a verb that runs with a flag missing (an empty address then reaches buck2's
    // config), or that accepts a flag it does not know.
    let dir = scratch("usage");
    let out = dir.join("farm.buckconfig");
    let all: Vec<String> = [
        "farm-config",
        "--address",
        ADDRESS,
        "--instance",
        "main",
        "--tls",
        "true",
        "--properties",
        PROPERTIES,
        "--out",
        out.to_str().expect("utf-8"),
    ]
    .map(str::to_owned)
    .to_vec();
    for flag in (1..all.len()).step_by(2) {
        let mut without = all.clone();
        without.drain(flag..flag + 2);
        let o = Command::new(XTASK).args(&without).output().expect("run");
        assert_eq!(o.status.code(), Some(2), "{without:?}");
        assert_eq!(stdout(&o), "");
        assert!(stderr(&o).contains("required"), "{}", stderr(&o));
    }
    let mut unknown = all.clone();
    unknown.extend(["--nope".to_owned(), "x".to_owned()]);
    let o = Command::new(XTASK).args(&unknown).output().expect("run");
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--nope"), "{}", stderr(&o));
    assert!(!out.exists());
}
