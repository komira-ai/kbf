//! The farm's processes start from a generated `kbf.toml`, as the packaged units start
//! `kbf`; they used to start from the flags of the unit `kbf-deploy` rendered. This holds
//! the new to the old: for every process of every topology the farm is started in, `kbf`'s
//! own parser reads the new command line and file as it read the old flags.
//!
//! The reference is `tests/golden/farm-units.txt`: per process, its mode and the flags the rendered unit gave it after the farm's edits (recorded from the old
//! path, run beside the new one, when the farm moved). A flag the farm would start without,
//! with another value, or in another mode fails here by name, and so does a parsed default
//! that differs.
//!
//! The file is not regenerated: a change of the flags a farm process starts with is an
//! intended change of the reference, and the reviewer sees it in the diff of the file.

use std::collections::BTreeMap;
use std::path::Path;

use kbf_harness::topology::{Limits, Topology};
use kbf_harness::units::{Mode, Settings, Unit, Wiring, frontend, keys, node};
use kbf_testkit::dir::TestDir;

/// The address of a role, as the farm gives them out: the frontend's is the first of the
/// loopback block, then each node's. Built at run time (the repository's hygiene scan allows
/// no other literal loopback address in a text file), so that a process told another's
/// address differs from the golden, which writes `{host}` for a process's own and
/// `{host:node-1}` for node 1's.
fn host_of(role: &str) -> String {
    let n = match role.split_once('-') {
        None => 0,
        Some((_, n)) => n.parse::<usize>().expect("a number"),
    };
    format!("127.0.0.{}", n + 1)
}

const GOLDEN: &str = include_str!("golden/farm-units.txt");

/// A process as the reference describes it.
#[derive(Debug, PartialEq, Eq)]
struct Process {
    mode: String,
    flags: Vec<String>,
}

fn wiring() -> Wiring {
    Wiring {
        reapi: 41000,
        worker: 41001,
        api: 41002,
        storage_url: format!("https://{}:41010", host_of("node-1")),
    }
}

/// The topologies a farm is started in: the rehearsal's (two nodes, the cold store's floor
/// only), the same under a hosted runner's limits, and the beta farm's (three nodes, a hosted
/// runner's limits).
fn topologies() -> Vec<(&'static str, Topology)> {
    let mut rehearsal_small = Topology::rehearsal(2);
    rehearsal_small.limits = Limits::small();
    vec![
        ("rehearsal", Topology::rehearsal(2)),
        ("rehearsal-small", rehearsal_small),
        ("beta", Topology::beta()),
    ]
}

/// Every process of `topo`, shaped by `shape`: role name, and what the farm starts it with.
fn processes<T>(topo: &Topology, shape: impl Fn(Unit) -> T) -> Vec<(String, T)> {
    let w = wiring();
    let place = |root: &'static str, host: &'_ str, port: u16| {
        // The settings borrow the address; it outlives them in the caller's frame.
        let host: &'static str = Box::leak(host.to_owned().into_boxed_str());
        Settings {
            root: Path::new(root),
            host,
            storage_port: port,
            wiring: &w,
        }
    };
    let mut out = Vec::new();
    let front = place("/farm/frontend", &host_of("frontend"), 41020);
    out.push(("frontend".to_owned(), shape(frontend(&front, topo))));
    let roots = ["/farm/node-1", "/farm/node-2", "/farm/node-3"];
    let ids = ["node-1", "node-2", "node-3"];
    for i in 0..topo.storage_nodes {
        let s = place(
            roots[i],
            &host_of(ids[i]),
            41010 + u16::try_from(i).expect("small"),
        );
        let unit = node(&s, 41030 + u16::try_from(i).expect("small"), topo);
        out.push((format!("node-{}", i + 1), shape(unit)));
    }
    out
}

/// The processes the golden records, by `<topology> <role>`.
fn golden() -> BTreeMap<String, Process> {
    let mut found = BTreeMap::new();
    let mut current: Option<(String, Process)> = None;
    for line in GOLDEN.lines() {
        if let Some(name) = line.strip_prefix("== ") {
            found.extend(current.take());
            let p = Process {
                mode: String::new(),
                flags: Vec::new(),
            };
            current = Some((name.to_owned(), p));
            continue;
        }
        let (_, p) = current.as_mut().expect("a section first");
        if let Some(mode) = line.strip_prefix("mode ") {
            mode.clone_into(&mut p.mode);
        } else {
            p.flags.push(line.to_owned());
        }
    }
    found.extend(current);
    // The placeholders are the process's own address and node 1's.
    for (name, p) in &mut found {
        let (_, role) = name.split_once(' ').expect("topology and role");
        for flag in &mut p.flags {
            *flag = flag
                .replace("{host:node-1}", &host_of("node-1"))
                .replace("{host}", &host_of(role));
        }
    }
    found
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("harness-equiv-{name}"),
    )
}

/// Catches a farm process that would start with other settings than before: for every
/// process of every topology, the command line and `kbf.toml` the harness writes parse, by
/// `kbf`'s own parser, to the same flags (every default included) as the rendered unit's
/// flags did.
#[test]
fn the_generated_kbf_toml_parses_to_what_the_rendered_flags_parsed_to() {
    let dir = scratch("parse");
    let empty_dir = scratch("empty");
    let empty = kbf_testkit::config::flag(empty_dir.path());
    let old = golden();
    let mut compared = 0;
    for (name, topo) in topologies() {
        for (role, parsed) in processes(&topo, |unit| {
            let config = kbf_testkit::config::flag_with(dir.path(), &unit.toml());
            let argv = unit.argv(config);
            // The file is read by the parse that follows, so write it before.
            let parsed = kbf_cli::parse_from(std::iter::once("kbf".to_owned()).chain(argv.clone()))
                .unwrap_or_else(|e| panic!("kbf refuses {argv:?}:\n{e}\n{}", unit.toml()));
            format!("{parsed:?}")
        }) {
            let reference = &old[&format!("{name} {role}")];
            let words: Vec<String> = ["kbf".to_owned(), reference.mode.clone()]
                .into_iter()
                .chain(reference.flags.iter().cloned())
                .chain([empty.clone()])
                .collect();
            let want = kbf_cli::parse_from(words.clone())
                .unwrap_or_else(|e| panic!("kbf refuses the old {words:?}:\n{e}"));
            assert_eq!(parsed, format!("{want:?}"), "{name} {role}");
            compared += 1;
        }
    }
    // 3 + 3 + 4 processes: the frontend and the nodes of each topology.
    assert_eq!(compared, old.len());
    assert!(compared > 8, "{compared}");
}

/// Catches a key of the harness's tables that `kbf` does not take in that mode or reads in
/// another table (the harness reads `tests/golden/kbf-<mode>.toml`, a text that `kbf-cli`
/// holds equal to the flags): a file of every key of every mode, a switch on and the rest
/// text, is accepted by `kbf`'s loader.
#[test]
fn every_key_the_harness_knows_is_a_key_kbf_takes_in_its_group() {
    let dir = scratch("keys");
    for mode in Mode::ALL {
        let mut text = String::new();
        let mut by_table: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (key, (table, switch)) in keys(mode) {
            assert_eq!(kbf_cli::config::group_of(key), Some(table), "{mode} {key}");
            let value = if switch { "true" } else { "\"x\"" };
            by_table
                .entry(table)
                .or_default()
                .push(format!("{key} = {value}\n"));
        }
        for (table, lines) in by_table {
            text.push_str(&format!("[{table}]\n{}\n", lines.concat()));
        }
        let path = dir.path().join(format!("{mode}.toml"));
        std::fs::write(&path, &text).expect("write");
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("mode");
        if let Err(e) = kbf_cli::check_settings(mode.name(), &path) {
            panic!("{mode}: kbf refuses a key of the harness's table:\n{e}");
        }
    }
}
