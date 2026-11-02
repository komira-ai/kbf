//! The example units in `docs/deploy/*.md` against the code (`kbf_integration_test::deploy_units`
//! reads them).
//!
//! Catches:
//! - a server unit whose `ExecStart=` names a flag `kbf-server` does not have, gives one
//!   a value it refuses, or leaves out a flag another requires (a deny list without
//!   mutual TLS);
//! - a unit that starts a binary neither this test nor `kbf-node`'s
//!   (`src/deploy_doc_tests.rs`) parses, so its flags would go unchecked;
//! - a flag `storage-node.md` lists as planned that `kbf-server` now accepts, so the doc
//!   still calls built code planned;
//! - a converged worked example that does not add up, or whose result is not the
//!   converged daemon's `--actions-memory-max-gib`; a reservation that is not its unit's
//!   `MemoryMin=`; a `kbf.slice` whose `MemoryMin=` is not the sum of its units';
//! - units that break the priority order MinIO > `kbf-server` > `kbf-daemon` in
//!   `OOMScoreAdjust=` or `CPUWeight=`;
//! - a `kbf cluster ...` command line in a deploy doc (`switch-object-store.md`'s
//!   commands and its held script among them) whose verb or flag `kbf cluster` does not
//!   have, or that leaves out a flag the verb requires;
//! - the checks passing vacuously because the docs or their units moved.

use std::path::{Path, PathBuf};

use clap::Parser as _;
use clap::error::ErrorKind;
use kbf_cli::cluster::Verb;
use kbf_cli::cluster::object_store::ObjectStore;
use kbf_integration_test::deploy_units::{
    ClusterCommand, Unit, cluster_commands, fenced, table_after, units,
};
use kbf_server::Args;

/// The binaries a unit may start: this test parses `kbf-server`'s flags, `kbf-node`'s
/// tests parse `kbf-daemon`'s.
const BINARIES: &[&str] = &["/usr/local/bin/kbf-server", "/usr/local/bin/kbf-daemon"];

fn deploy_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/deploy")
}

/// Every deploy doc, by file name, with its units.
fn docs() -> Vec<(String, String, Vec<Unit>)> {
    let mut out: Vec<_> = std::fs::read_dir(deploy_dir())
        .expect("docs/deploy")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .map(|p| {
            let name = p.file_name().expect("name").to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&p).expect("read doc");
            let units = units(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, text, units)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn storage_node() -> (String, Vec<Unit>) {
    let (_, text, units) = docs()
        .into_iter()
        .find(|(n, _, _)| n == "storage-node.md")
        .expect("docs/deploy/storage-node.md");
    (text, units)
}

/// The one unit at `path` in section `section`.
fn unit<'a>(units: &'a [Unit], section: &str, path: &str) -> &'a Unit {
    let found: Vec<_> = units
        .iter()
        .filter(|u| u.section == section && u.path == path)
        .collect();
    assert_eq!(found.len(), 1, "{path} under \"## {section}\": {found:?}");
    found[0]
}

/// A `MemoryMin=` value in MiB (`G` or `M` suffix, as the docs write it).
fn mib(value: &str) -> u64 {
    let (n, unit) = value.split_at(value.len() - 1);
    let n: u64 = n.parse().unwrap_or_else(|_| panic!("MemoryMin={value}"));
    match unit {
        "G" => n * 1024,
        "M" => n,
        _ => panic!("MemoryMin={value}: write G or M"),
    }
}

fn memory_min(u: &Unit) -> u64 {
    mib(u
        .get("MemoryMin")
        .unwrap_or_else(|| panic!("{} has no MemoryMin=", u.path)))
}

#[test]
fn every_server_units_flags_parse_with_kbf_servers_parser() {
    // Catches: a misspelled or removed kbf-server flag in a doc, a value it refuses, a
    // missing required flag; a unit starting an unparsed binary; units that moved.
    let mut servers = 0;
    let mut daemons = 0;
    for (name, _, units) in docs() {
        for u in &units {
            let Some(argv) = u.exec_start() else { continue };
            let argv = argv.unwrap_or_else(|e| panic!("{name} {}: {e}", u.path));
            assert!(
                BINARIES.contains(&argv[0].as_str()),
                "{name} {}: {} is a binary no test parses the flags of",
                u.path,
                argv[0]
            );
            if argv[0].ends_with("kbf-daemon") {
                daemons += 1;
                continue;
            }
            servers += 1;
            if let Err(e) = Args::try_parse_from(&argv) {
                panic!("{name} {}: kbf-server refuses its ExecStart=:\n{e}", u.path);
            }
        }
    }
    // The server-only unit; the build host's and the converged host's daemons.
    assert_eq!(
        (servers, daemons),
        (1, 2),
        "units starting kbf-server, kbf-daemon"
    );
}

#[test]
fn the_planned_server_flags_are_still_refused() {
    // Catches: a planned flag that kbf-server now accepts while the doc still calls it
    // planned, and the planned block losing its info string (then nothing is checked).
    let (text, _) = storage_node();
    let planned = fenced(&text, "text planned-flags kbf-server");
    assert_eq!(planned.len(), 1, "one planned-flags block");
    assert_eq!(planned[0].len(), 1, "{:?}", planned[0]);
    for flag in &planned[0] {
        let kind = Args::try_parse_from(["kbf-server", flag])
            .map(|_| ())
            .map_err(|e| e.kind());
        assert_eq!(
            kind,
            Err(ErrorKind::UnknownArgument),
            "{flag} is accepted now: move it into the kbf-server unit and out of \
             storage-node.md's planned flags"
        );
    }
}

#[test]
fn the_converged_worked_example_adds_up_and_is_the_daemons_cap() {
    // Catches: one number in the worked example changed so it no longer adds up, the
    // converged daemon's cap not the example's result, a reservation that is not its
    // unit's MemoryMin=.
    let (text, units) = storage_node();
    let rows = table_after(&text, "### Worked example: a 256 GiB host");
    let gib = |label: &str| -> u64 {
        let row = rows
            .iter()
            .find(|r| r[0] == label)
            .unwrap_or_else(|| panic!("no row {label:?} in {rows:?}"));
        row[1].parse().unwrap_or_else(|_| panic!("{row:?}"))
    };
    let subtracted: Vec<u64> = rows
        .iter()
        .filter(|r| r[0].starts_with("minus "))
        .map(|r| r[1].parse().expect("GiB"))
        .collect();
    assert_eq!(subtracted.len(), 3, "MinIO, kbf-server, the OS: {rows:?}");
    let cap = gib("`--actions-memory-max-gib`");
    assert_eq!(gib("RAM") - subtracted.iter().sum::<u64>(), cap, "{rows:?}");

    let daemon = unit(
        &units,
        "Converged units",
        "/etc/systemd/system/kbf-daemon.service",
    );
    let flag = format!("--actions-memory-max-gib={cap}");
    let argv = daemon.exec_start().expect("ExecStart").expect("argv");
    assert!(
        argv.contains(&flag),
        "the converged daemon needs {flag}: {argv:?}"
    );

    // A reservation is the unit's MemoryMin=.
    let minio = unit(
        &units,
        "Server-only units",
        "/etc/systemd/system/minio.service.d/kbf.conf",
    );
    let server = unit(
        &units,
        "Server-only units",
        "/etc/systemd/system/kbf-server.service",
    );
    assert_eq!(memory_min(minio), gib("minus MinIO's reservation") * 1024);
    assert_eq!(
        memory_min(server),
        gib("minus `kbf-server`'s reservation") * 1024
    );
}

#[test]
fn each_slice_protects_the_sum_of_its_units() {
    // Catches: a slice MemoryMin= below its units' sum (the kernel caps their
    // protection at it) or above it (the excess goes to builds), and a unit outside
    // kbf.slice, whose MemoryMin= its slice would not cover.
    let (_, units) = storage_node();
    let slice = |section| memory_min(unit(&units, section, "/etc/systemd/system/kbf.slice"));
    let of = |section, path| memory_min(unit(&units, section, path));
    let minio = of(
        "Server-only units",
        "/etc/systemd/system/minio.service.d/kbf.conf",
    );
    let server = of(
        "Server-only units",
        "/etc/systemd/system/kbf-server.service",
    );
    let daemon = of("Converged units", "/etc/systemd/system/kbf-daemon.service");
    assert_eq!(slice("Server-only units"), minio + server);
    assert_eq!(slice("Converged units"), minio + server + daemon);
    for u in units.iter().filter(|u| !u.path.ends_with(".slice")) {
        assert_eq!(u.get("Slice"), Some("kbf.slice"), "{}", u.path);
    }
}

#[test]
fn the_units_keep_the_priority_order() {
    // Catches: an OOMScoreAdjust= or CPUWeight= edit that ranks the daemon above the
    // server or the server above MinIO, or the daemon at or above builds' 0.
    let (_, units) = storage_node();
    let order = [
        unit(
            &units,
            "Server-only units",
            "/etc/systemd/system/minio.service.d/kbf.conf",
        ),
        unit(
            &units,
            "Server-only units",
            "/etc/systemd/system/kbf-server.service",
        ),
        unit(
            &units,
            "Converged units",
            "/etc/systemd/system/kbf-daemon.service",
        ),
    ];
    let values = |key: &str| -> Vec<i64> {
        order
            .iter()
            .map(|u| u.get(key).and_then(|v| v.parse().ok()).expect(key))
            .collect()
    };
    let oom = values("OOMScoreAdjust");
    assert!(
        oom.is_sorted_by(|a, b| a < b),
        "OOMScoreAdjust= must rise: {oom:?}"
    );
    assert!(oom[2] < 0, "builds run at 0, below the daemon: {oom:?}");
    let cpu = values("CPUWeight");
    assert!(
        cpu.is_sorted_by(|a, b| a > b),
        "CPUWeight= must fall: {cpu:?}"
    );
}

/// Parses a doc's `kbf cluster` command with `kbf`'s own parser.
fn parse(c: &ClusterCommand) -> Result<kbf_cli::cluster::Cluster, clap::Error> {
    let argv: Vec<std::ffi::OsString> = c.argv.iter().map(Into::into).collect();
    kbf_cli::parse_cluster(&argv)
}

#[test]
fn every_kbf_cluster_command_in_the_deploy_docs_parses_with_kbfs_parser() {
    // Catches: a misspelled or removed verb or flag of `kbf cluster` in a deploy doc's
    // command lines or held script, a required flag left out, a value the verb refuses,
    // and the check passing vacuously because the commands moved.
    let mut pages = Vec::new();
    for (name, text, _) in docs() {
        let commands = cluster_commands(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        for c in &commands {
            if let Err(e) = parse(c) {
                panic!(
                    "{name} line {}: kbf cluster refuses `{}`:\n{e}",
                    c.line,
                    c.argv.join(" ")
                );
            }
        }
        if !commands.is_empty() {
            pages.push(name);
        }
    }
    assert_eq!(
        pages,
        ["switch-object-store.md", "update-a-host.md"],
        "the deploy docs with kbf cluster command lines"
    );
}

#[test]
fn the_object_store_switch_page_has_every_step_of_the_walk() {
    // Catches: a step of the switch (the repoint, safe-to-stop, drain, planned-down,
    // the generation check, undrain) dropped from the page or from its held script, or
    // the page's blocks losing their fences so nothing of it is checked.
    let (_, text, _) = docs()
        .into_iter()
        .find(|(n, _, _)| n == "switch-object-store.md")
        .expect("docs/deploy/switch-object-store.md");
    let verbs: Vec<Verb> = cluster_commands(&text)
        .expect("commands")
        .iter()
        .map(|c| parse(c).expect("parsed by the test above").verb)
        .collect();
    let count = |is: &dyn Fn(&Verb) -> bool| verbs.iter().filter(|v| is(v)).count();
    // Each step is in the page's walk and again in the held script.
    let steps: [(&str, usize); 6] = [
        (
            "repoint",
            count(&|v| {
                matches!(
                    v,
                    Verb::ObjectStore {
                        verb: ObjectStore::Repoint { .. }
                    }
                )
            }),
        ),
        (
            "status",
            count(&|v| {
                matches!(
                    v,
                    Verb::ObjectStore {
                        verb: ObjectStore::Status { .. }
                    }
                )
            }),
        ),
        (
            "safe-to-stop",
            count(&|v| matches!(v, Verb::SafeToStop { .. })),
        ),
        (
            "drain",
            count(&|v| {
                matches!(
                    v,
                    Verb::Drain {
                        frontend: false,
                        ..
                    }
                )
            }),
        ),
        (
            "planned-down",
            count(&|v| {
                matches!(
                    v,
                    Verb::Member {
                        for_secs: Some(_),
                        ..
                    }
                )
            }),
        ),
        ("undrain", count(&|v| matches!(v, Verb::Undrain { .. }))),
    ];
    for (step, n) in steps {
        assert!(
            n >= 2,
            "{step}: {n} command line(s), want the page's and the script's"
        );
    }
}

#[test]
fn a_misspelled_flag_or_verb_on_a_page_is_refused() {
    // Catches: a parse that accepts what `kbf cluster` does not have, so a wrong page
    // would pass the test above.
    let page = "\
```text
kbf cluster object-store repoint --connection <connection> --endpoint <new endpoint URL> --expect-generaton <generation> --token-file /etc/kbf/api-token
kbf cluster object-store repiont --connection <connection> --endpoint <new endpoint URL> --expect-generation <generation> --token-file /etc/kbf/api-token
kbf cluster object-store repoint --connection <connection> --endpoint <new endpoint URL> --expect-generation <generation>
kbf cluster drain \"$node_id\" --tokenfile \"$TOKEN_FILE\"
```
";
    let kinds: Vec<ErrorKind> = cluster_commands(page)
        .expect("commands")
        .iter()
        .map(|c| parse(c).expect_err("refused").kind())
        .collect();
    assert_eq!(
        kinds,
        [
            ErrorKind::UnknownArgument,
            ErrorKind::InvalidSubcommand,
            ErrorKind::MissingRequiredArgument,
            ErrorKind::UnknownArgument,
        ]
    );
}
