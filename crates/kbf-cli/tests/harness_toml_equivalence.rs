//! The farm's processes start from a generated `kbf.toml`, as the packaged units start
//! `kbf`; they used to start from the flags of the unit `kbf-deploy` renders. This holds
//! the one to the other: for every process of every topology the farm is started in, `kbf`'s
//! own parser reads the new command line and file as it read the old flags.
//!
//! The reference is `tests/golden/farm-units.txt`: per process, its mode, its environment
//! and the flags the rendered unit gave it after the farm's edits (the old path's
//! output, recorded while both paths existed and held to the live old path then). A flag
//! the farm would start without, with another value, or in another mode fails here by
//! name, and so does a `Debug` of the parsed flags that differs in a default.
//!
//! To rewrite the golden after an intended change of the farm's flags:
//! `BLESS=1 cargo test -p kbf-cli --test harness_toml_equivalence`.

use std::collections::BTreeMap;
use std::path::Path;

use kbf_harness::topology::{Limits, Role, Topology};
use kbf_harness::units::{
    Container, Edits, Mode, Settings, Unit, Wiring, daemon, frontend, keys, node,
};
use kbf_harness::units_rendered::Rendered;
use kbf_testkit::dir::TestDir;

/// The image the container driver's daemons are pinned to (a 64-digit digest).
const IMAGE: &str =
    "registry.test/busybox@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

const GOLDEN: &str = include_str!("golden/farm-units.txt");

/// A process as a reference describes it.
#[derive(Debug, PartialEq, Eq)]
struct Process {
    mode: String,
    env: Vec<(String, String)>,
    flags: Vec<String>,
}

fn wiring() -> Wiring {
    Wiring {
        reapi: 41000,
        worker: 41001,
        api: 41002,
        storage_url: "https://127.0.0.2:41010".to_owned(),
    }
}

/// The flags `b0`'s mutants give a node: a cold store that holds nothing for long.
fn shrunk() -> Vec<String> {
    vec![
        format!("--cold-store-size-bytes={}", 1 << 20),
        "--cold-store-floor-bytes=0".to_owned(),
        "--cold-store-evict-grace-ms=0".to_owned(),
        format!("--hot-store-max-bytes={}", 1 << 20),
    ]
}

/// The topologies a farm is started in: the rehearsal's (container driver, storage and
/// daemon in one process), the beta farm's (native driver, a hosted runner's limits),
/// both with the overrides the beta scenario and its tests give.
fn topologies() -> Vec<(&'static str, Topology)> {
    let mut rehearsal_small = Topology::rehearsal(2);
    rehearsal_small.limits = Limits::small();
    let mut beta_mutant = Topology::beta();
    beta_mutant.overrides = vec![
        (Role::Node(1), shrunk()),
        // `b0`'s `extra_flags` in the test of a farm that never becomes ready.
        (
            Role::Daemon(1),
            vec!["--server=https://localhost:1".to_owned()],
        ),
    ];
    let mut beta_every = Topology::beta();
    beta_every.overrides = (1..=3).map(|n| (Role::Node(n), shrunk())).collect();
    vec![
        ("rehearsal", Topology::rehearsal(2)),
        ("rehearsal-small", rehearsal_small),
        ("beta", Topology::beta()),
        ("beta-mutant", beta_mutant),
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

/// Every process of `topo`, built by `U`: role name, and what the farm starts it with.
fn processes<U: Edits, T>(topo: &Topology, shape: impl Fn(U) -> T) -> Vec<(String, T)> {
    let w = wiring();
    let uid = 1000;
    let native = topo.driver == kbf_harness::topology::Driver::Native;
    let place = |root: &'static str, host: &'static str, id: &'static str, port: u16| Settings {
        root: Path::new(root),
        host,
        node_id: id,
        storage_port: port,
        wiring: &w,
        image: if native { "" } else { IMAGE },
    };
    let mut out = Vec::new();
    let front = Settings {
        image: "",
        ..place("/farm/frontend", "127.0.0.1", "frontend-1", 41020)
    };
    let socket = Path::new("/tmp/kbf-farm-1.sock");
    out.push((
        "frontend".to_owned(),
        shape(frontend::<U>(&front, uid, socket, topo)),
    ));
    let roots = ["/farm/node-1", "/farm/node-2", "/farm/node-3"];
    let hosts = ["127.0.0.2", "127.0.0.3", "127.0.0.4"];
    let ids = ["node-1", "node-2", "node-3"];
    for i in 0..topo.storage_nodes {
        let s = place(
            roots[i],
            hosts[i],
            ids[i],
            41010 + u16::try_from(i).expect("small"),
        );
        let c = (!native).then(|| container(roots[i]));
        let unit = node::<U>(
            (&s, uid),
            i + 1,
            41030 + u16::try_from(i).expect("small"),
            topo,
            c.as_ref(),
        );
        out.push((format!("node-{}", i + 1), shape(unit)));
    }
    let daemons = ["/farm/daemon-1", "/farm/daemon-2"];
    let dhosts = ["127.0.0.5", "127.0.0.6"];
    let dids = ["daemon-1", "daemon-2"];
    for d in 0..topo.daemons {
        let s = place(daemons[d], dhosts[d], dids[d], 41020);
        let c = (!native).then(|| container(daemons[d]));
        let unit = daemon::<U>((&s, uid), d + 1, topo, c.as_ref());
        out.push((format!("daemon-{}", d + 1), shape(unit)));
    }
    out
}

/// The old path's processes, as the golden records them.
fn recorded(topo: &Topology) -> Vec<(String, Process)> {
    processes::<Rendered, _>(topo, |unit| {
        let mut flags: Vec<String> = unit.argv[1..].to_vec();
        flags.sort();
        Process {
            mode: unit.argv[0].clone(),
            env: unit.env.clone(),
            flags,
        }
    })
}

/// The golden text of every topology's processes.
fn golden_text(all: &[(&str, Vec<(String, Process)>)]) -> String {
    let mut out = String::new();
    for (topo, procs) in all {
        for (role, p) in procs {
            out.push_str(&format!("== {topo} {role}\nmode {}\n", p.mode));
            for (k, v) in &p.env {
                out.push_str(&format!("env {k}={v}\n"));
            }
            for flag in &p.flags {
                out.push_str(&format!("{flag}\n"));
            }
        }
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
    found
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("harness-equiv-{name}"),
    )
}

/// Catches the reference drifting from the old path while both exist, and (with BLESS) writes
/// it. With the old path gone the golden is the reference itself.
#[test]
fn the_golden_is_what_the_rendered_units_gave_the_farm() {
    let all: Vec<(&str, Vec<(String, Process)>)> = topologies()
        .iter()
        .map(|(name, topo)| (*name, recorded(topo)))
        .collect();
    let text = golden_text(&all);
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/farm-units.txt"),
            &text,
        )
        .expect("bless");
        return;
    }
    assert!(
        text == GOLDEN,
        "differs from tests/golden/farm-units.txt:\n{text}"
    );
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
        for (role, (parsed, env)) in processes::<Unit, _>(&topo, |unit: Unit| {
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
