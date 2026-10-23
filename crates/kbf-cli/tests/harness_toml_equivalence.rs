//! The farm's processes start from a generated `kbf.toml`, as the packaged units start
//! `kbf`; they used to start from the flags of the unit `kbf-deploy` rendered. This holds
//! the new to the old: for every process of every topology the farm is started in, `kbf`'s
//! own parser reads the new command line and file as it read the old flags.
//!
//! The reference is `tests/golden/farm-units.txt`: per process, its mode, its environment
//! and the flags the rendered unit gave it after the farm's edits (recorded from the old
//! path, run beside the new one, when the farm moved). A flag the farm would start without,
//! with another value, or in another mode fails here by name, and so does a parsed default
//! that differs.
//!
//! The file is not regenerated: a change of the flags a farm process starts with is an
//! intended change of the reference, and the reviewer sees it in the diff of the file.

use std::collections::BTreeMap;
use std::path::Path;

use kbf_harness::topology::{Limits, Role, Topology};
use kbf_harness::units::{Container, Mode, Settings, Unit, Wiring, daemon, frontend, keys, node};
use kbf_testkit::dir::TestDir;

/// The image the container driver's daemons are pinned to (a 64-digit digest).
const IMAGE: &str =
    "registry.test/busybox@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

/// The address of a role, as the farm gives them out: the frontend's is the first of the
/// loopback block, then each node's, then each plain daemon's. Built at run time (the
/// repository's hygiene scan allows no other literal loopback address in a text file), so
/// that a process told another's address differs from the golden, which writes `{host}`
/// for a process's own and `{host:node-1}` for node 1's.
fn host_of(role: &str, nodes: usize) -> String {
    let n = match role.split_once('-') {
        None => 0,
        Some(("node", n)) => n.parse::<usize>().expect("a number"),
        Some((_, d)) => nodes + d.parse::<usize>().expect("a number"),
    };
    format!("127.0.0.{}", n + 1)
}

const GOLDEN: &str = include_str!("golden/farm-units.txt");

/// A process as the reference describes it.
#[derive(Debug, PartialEq, Eq)]
struct Process {
    mode: String,
    env: Vec<(String, String)>,
    flags: Vec<String>,
}

fn wiring(nodes: usize) -> Wiring {
    Wiring {
        reapi: 41000,
        worker: 41001,
        api: 41002,
        storage_url: format!("https://{}:41010", host_of("node-1", nodes)),
    }
}

/// The topologies a farm is started in: the rehearsal's (container driver, storage and
/// daemon in one process), the beta farm's (native driver, a hosted runner's limits),
/// both with the overrides the beta scenario and its tests give.
fn topologies() -> Vec<(&'static str, Topology)> {
    let mut rehearsal_small = Topology::rehearsal(2);
    rehearsal_small.limits = Limits::small();
    let mut beta_shrunk = Topology::beta();
    beta_shrunk.overrides = vec![
        (Role::Node(1), kbf_harness::b0::shrunk()),
        // `b0`'s `extra_flags` in the test of a farm that never becomes ready.
        (
            Role::Daemon(1),
            vec!["--server=https://localhost:1".to_owned()],
        ),
    ];
    let mut beta_every = Topology::beta();
    beta_every.overrides = (1..=3)
        .map(|n| (Role::Node(n), kbf_harness::b0::shrunk()))
        .collect();
    vec![
        ("rehearsal", Topology::rehearsal(2)),
        ("rehearsal-small", rehearsal_small),
        ("beta", Topology::beta()),
        ("beta-shrunk", beta_shrunk),
        ("beta-every", beta_every),
    ]
}

/// The edits the box's container driver makes: the stand-in `crun`, a planted cgroup tree
/// and the id files (the names `Container::planted` gives, at fixed paths).
fn container(root: &str) -> Container {
    Container {
        oci_runtime: format!("--oci-runtime={root}/bin/crun"),
        flags: vec![
            format!("--cgroup-root={root}/bin/cgroup"),
            format!("--self-cgroup={root}/bin/cgroup/kbf"),
            format!("--id-files={root}/ids"),
        ],
    }
}

/// Every process of `topo`, shaped by `shape`: role name, and what the farm starts it with.
fn processes<T>(topo: &Topology, shape: impl Fn(Unit) -> T) -> Vec<(String, T)> {
    let nodes = topo.storage_nodes;
    let w = wiring(nodes);
    let uid = 1000;
    let native = topo.driver == kbf_harness::topology::Driver::Native;
    let place = |root: &'static str, host: &'_ str, id: &'static str, port: u16| {
        // The settings borrow the address; it outlives them in the caller's frame.
        let host: &'static str = Box::leak(host.to_owned().into_boxed_str());
        Settings {
            root: Path::new(root),
            host,
            node_id: id,
            storage_port: port,
            wiring: &w,
            image: if native { "" } else { IMAGE },
        }
    };
    let mut out = Vec::new();
    let front = Settings {
        image: "",
        ..place(
            "/farm/frontend",
            &host_of("frontend", nodes),
            "frontend-1",
            41020,
        )
    };
    let socket = Path::new("/tmp/kbf-farm-1.sock");
    out.push((
        "frontend".to_owned(),
        shape(frontend(&front, uid, socket, topo)),
    ));
    let roots = ["/farm/node-1", "/farm/node-2", "/farm/node-3"];
    let ids = ["node-1", "node-2", "node-3"];
    for i in 0..nodes {
        let s = place(
            roots[i],
            &host_of(ids[i], nodes),
            ids[i],
            41010 + u16::try_from(i).expect("small"),
        );
        let c = (!native).then(|| container(roots[i]));
        let unit = node(
            (&s, uid),
            i + 1,
            41030 + u16::try_from(i).expect("small"),
            topo,
            c.as_ref(),
        );
        out.push((format!("node-{}", i + 1), shape(unit)));
    }
    let daemons = ["/farm/daemon-1", "/farm/daemon-2"];
    let dids = ["daemon-1", "daemon-2"];
    for d in 0..topo.daemons {
        let s = place(daemons[d], &host_of(dids[d], nodes), dids[d], 41020);
        let c = (!native).then(|| container(daemons[d]));
        let unit = daemon((&s, uid), d + 1, topo, c.as_ref());
        out.push((format!("daemon-{}", d + 1), shape(unit)));
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
                env: Vec::new(),
                flags: Vec::new(),
            };
            current = Some((name.to_owned(), p));
            continue;
        }
        let (_, p) = current.as_mut().expect("a section first");
        if let Some(mode) = line.strip_prefix("mode ") {
            mode.clone_into(&mut p.mode);
        } else if let Some(env) = line.strip_prefix("env ") {
            let (k, v) = env.split_once('=').expect("K=V");
            p.env.push((k.to_owned(), v.to_owned()));
        } else {
            p.flags.push(line.to_owned());
        }
    }
    found.extend(current);
    // The placeholders are the process's own address and node 1's.
    let nodes: BTreeMap<&str, usize> = topologies()
        .iter()
        .map(|(name, topo)| (*name, topo.storage_nodes))
        .collect();
    for (name, p) in &mut found {
        let (topo, role) = name.split_once(' ').expect("topology and role");
        let n = nodes[topo];
        for flag in &mut p.flags {
            *flag = flag
                .replace("{host:node-1}", &host_of("node-1", n))
                .replace("{host}", &host_of(role, n));
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
/// flags did, and the environment is the same.
#[test]
fn the_generated_kbf_toml_parses_to_what_the_rendered_flags_parsed_to() {
    let dir = scratch("parse");
    let empty_dir = scratch("empty");
    let empty = kbf_testkit::config::flag(empty_dir.path());
    let old = golden();
    let mut compared = 0;
    for (name, topo) in topologies() {
        for (role, (parsed, env)) in processes(&topo, |unit| {
            let config = kbf_testkit::config::flag_with(dir.path(), &unit.toml());
            let argv = unit.argv(config);
            // The file is read by the parse that follows, so write it before.
            let parsed = kbf_cli::parse_from(std::iter::once("kbf".to_owned()).chain(argv.clone()))
                .unwrap_or_else(|e| panic!("kbf refuses {argv:?}:\n{e}\n{}", unit.toml()));
            (format!("{parsed:?}"), unit.env.clone())
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
            assert_eq!(env, reference.env, "{name} {role}");
            compared += 1;
        }
    }
    // 4 + 4 + 5 + 5 + 6 processes: the frontend, the nodes and the daemons of each topology.
    assert_eq!(compared, old.len());
    assert!(compared > 20, "{compared}");
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
