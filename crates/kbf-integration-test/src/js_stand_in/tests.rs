//! The stand-in programs over a directory of the test's own: each answer, each way a call
//! can be wrong, and what the container tells the test.

use std::io::{BufRead as _, BufReader, Read as _};
use std::os::unix::net::UnixListener;
use std::path::Path;

use kbf_testkit::dir::TestDir;
use serde_json::json;

use super::*;

/// A place in a directory of its own: `stand-in/` and a cgroup mount that is a directory.
fn place(name: &str) -> (TestDir, Place) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let place = Place {
        dir: dir.path().join("stand-in"),
        cgroups: dir.path().join("cgroup"),
    };
    std::fs::create_dir_all(place.dir.join("plan")).expect("the stand-in directory");
    (dir, place)
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn call(place: &Place, argv0: &str, words: &[&str]) -> Answer {
    answer(argv0, &args(words), &mut std::io::empty(), place)
}

fn calls(place: &Place) -> String {
    std::fs::read_to_string(place.dir.join("calls")).expect("calls")
}

fn failed(stderr: &str) -> Answer {
    Answer {
        stderr: stderr.to_owned(),
        code: 1,
        ..Answer::default()
    }
}

fn exited(code: u8, stderr: &str) -> Answer {
    Answer {
        stderr: stderr.to_owned(),
        code,
        ..Answer::default()
    }
}

/// Asserts that every run is recorded under the name argv0 ends in, with its arguments, and
/// that `crun --version` names a release the agent takes. Catches a stand-in the test could
/// not hold to what the agent asked of it.
#[test]
fn every_call_is_recorded_and_crun_names_its_version() {
    let (_dir, place) = place("stand-in-version");
    assert_eq!(
        call(&place, "/node/bin/crun", &["--version"]),
        Answer::ok(VERSION)
    );
    assert_eq!(
        call(&place, "nsenter", &["--net=/proc/1/fd/3", "--", "ip"]),
        Answer::default()
    );
    assert_eq!(
        calls(&place),
        "crun --version\nnsenter --net=/proc/1/fd/3 -- ip\n"
    );
}

/// Asserts that a run that cannot record its call fails, saying so, and that a name that is
/// none of the four fails. Catches a stand-in whose calls go unseen.
#[test]
fn an_unrecorded_call_and_an_unknown_program_fail() {
    let (_dir, place) = place("stand-in-unknown");
    let lost = Place {
        dir: place.dir.join("missing"),
        ..place.clone()
    };
    let said = call(&lost, "ip", &["link"]);
    assert_eq!(said.code, 1);
    assert!(
        said.stderr.starts_with("ip: recording the call: "),
        "{said:?}"
    );
    assert_eq!(
        call(&place, "runc", &["run"]),
        failed("kbf-js-stand-in: no program \"runc\"")
    );
    assert_eq!(
        call(&place, "", &[]),
        failed("kbf-js-stand-in: no program \"\"")
    );
}

/// Asserts what `ip` and `nft` answer: no link, an empty table, a load recorded, and every
/// other call a success that reads its input. Catches an egress stand-in the agent's start
/// or its check before each create would refuse.
#[test]
fn ip_and_nft_answer_as_a_host_with_nothing_wired() {
    let (_dir, place) = place("stand-in-ip-nft");
    assert_eq!(
        call(&place, "ip", &["-j", "link", "show"]),
        Answer::ok("[]")
    );
    let mut batch = "link set lo up\n".as_bytes();
    assert_eq!(
        answer("ip", &args(&["-batch", "-"]), &mut batch, &place),
        Answer::default()
    );
    assert!(batch.is_empty(), "the batch is read");
    let table = call(&place, "nft", &["-j", "list", "table", "inet", "t"]);
    assert_eq!(table, Answer::ok(EMPTY_TABLE));
    let listing: Value = serde_json::from_str(&table.stdout).expect("JSON");
    assert_eq!(listing["nftables"][0]["metainfo"]["json_schema_version"], 1);
    assert_eq!(
        call(&place, "nft", &["add", "element", "inet", "t", "s"]),
        Answer::default()
    );
    for text in ["table inet t {}\n", "table ip n {}\n"] {
        let said = answer("nft", &args(&["-f", "-"]), &mut text.as_bytes(), &place);
        assert_eq!(said, Answer::default());
    }
    let loaded = std::fs::read_to_string(place.dir.join("nft-loaded")).expect("loaded");
    assert_eq!(loaded, "table inet t {}\ntable ip n {}\n");
    std::fs::remove_file(place.dir.join("nft-loaded")).expect("removed");
    std::fs::create_dir(place.dir.join("nft-loaded")).expect("a directory in its place");
    let said = answer("nft", &args(&["-f", "-"]), &mut "x".as_bytes(), &place);
    assert_eq!(said.code, 1);
    assert!(said.stderr.starts_with("nft: "), "{said:?}");
}

/// Asserts that `crun` reads its global options before the command (`--root`, `--log` and
/// `--log-format` with a value, `--debug` without), that `delete` succeeds, and that a
/// command it does not know, a command with no `--root` and no command at all fail naming
/// the call. Catches a stand-in that takes an option's value for the command.
#[test]
fn crun_reads_its_global_options_and_refuses_what_it_does_not_know() {
    let (_dir, place) = place("stand-in-crun-options");
    let delete = [
        "--root",
        "/r",
        "--log",
        "/l",
        "--log-format",
        "json",
        "--debug",
        "delete",
        "--force",
        "c1",
    ];
    assert_eq!(call(&place, "crun", &delete), Answer::default());
    assert_eq!(
        call(&place, "crun", &["--root", "/r", "pause", "c1"]),
        failed("crun: unknown command: --root /r pause c1")
    );
    assert_eq!(
        call(&place, "crun", &["delete", "c1"]),
        failed("crun: unknown command: delete c1")
    );
    assert_eq!(
        call(&place, "crun", &["--root", "/r", "--debug"]),
        failed("crun: unknown command: --root /r --debug")
    );
    assert_eq!(
        call(&place, "crun", &["--root", "/r", "state"]),
        failed("crun: unknown command: --root /r state")
    );
}

/// Asserts that `crun state` names the process of `netns-pid` as the running container's
/// first process, and fails while there is none. Catches a state the agent's wiring would
/// read as the host's own network namespace.
#[test]
fn crun_state_names_the_process_the_test_keeps() {
    let (_dir, place) = place("stand-in-state");
    let said = call(&place, "crun", &["--root", "/r", "state", "c1"]);
    assert_eq!(said.code, 1);
    assert!(
        said.stderr.starts_with("crun state: netns-pid: "),
        "{said:?}"
    );
    std::fs::write(place.dir.join("netns-pid"), "4321\n").expect("written");
    let said = call(&place, "crun", &["--root", "/r", "state", "c1"]);
    assert_eq!(said.code, 0);
    let state: Value = serde_json::from_str(&said.stdout).expect("JSON");
    assert_eq!(
        state,
        json!({"ociVersion": "1.0.2", "id": "c1", "pid": 4321, "status": "running"})
    );
}

/// Asserts that `crun kill` sends the signal it is given to the process `run` recorded
/// under `--root` (here this test's own, with a signal that does nothing), and fails when no
/// process was recorded, the signal has no name, or the process is gone. Catches a cancel
/// that never reaches the container.
#[test]
fn crun_kill_signals_the_recorded_process() {
    let (dir, place) = place("stand-in-kill");
    let root = dir.path().join("root");
    let root_arg = root.display().to_string();
    let kill = |signal: &str| call(&place, "crun", &["--root", &root_arg, "kill", "c1", signal]);
    assert_eq!(
        kill("15"),
        failed("crun kill: no container to signal: c1 15")
    );
    std::fs::create_dir_all(&root).expect("the root");
    std::fs::write(root.join("pid"), std::process::id().to_string()).expect("the pid");
    // SIGCHLD, whose default is to do nothing.
    assert_eq!(kill("17"), Answer::default());
    assert_eq!(
        kill("term"),
        failed("crun kill: no container to signal: c1 term")
    );
    // A pid above the kernel's largest (2^22) names no process.
    std::fs::write(root.join("pid"), "4194305").expect("the pid");
    let said = kill("17");
    assert_eq!(said.code, 1);
    assert!(said.stderr.starts_with("crun kill: "), "{said:?}");
}

/// A bundle under `dir` whose `config.json` is `config`.
fn bundle_at(dir: &Path, name: &str, config: &Value) -> String {
    let bundle = dir.join(name);
    std::fs::create_dir_all(&bundle).expect("the bundle");
    std::fs::write(bundle.join("config.json"), config.to_string()).expect("the config");
    bundle.display().to_string()
}

/// A job's config: the init, the entrypoint and the task tool's arguments for the action
/// of `hash`, in cgroup `cgroup`.
fn job_config(hash: &str, cgroup: &str) -> Value {
    json!({
        "process": {"args": ["/dev/js-init", "--", "/bin/task", "--action", format!("{hash}/7"), "--lease", "1-2"]},
        "linux": {"cgroupsPath": cgroup},
    })
}

/// Asserts that `crun run` refuses a call with no bundle or no name, that a helper's bundle
/// runs nothing, and that a job's bundle it cannot read, or that names no arguments or no
/// cgroup, fails as crun does when it cannot create a container (125). Catches a stand-in
/// that runs a job it could not read as one that succeeded.
#[test]
fn crun_run_refuses_a_bundle_it_cannot_read() {
    let (dir, place) = place("stand-in-bundles");
    let root = dir.path().join("root").display().to_string();
    let run = |bundle: &str| {
        call(
            &place,
            "crun",
            &["--root", &root, "run", "--bundle", bundle, "c1"],
        )
    };
    assert_eq!(
        call(&place, "crun", &["--root", &root, "run", "--bundle"]),
        failed("crun run: needs --bundle and a name")
    );
    let helper = bundle_at(dir.path(), "helper-3", &json!({}));
    assert_eq!(run(&helper), Answer::default());
    let missing = dir.path().join("none").display().to_string();
    let said = run(&missing);
    assert_eq!(said.code, 125);
    assert!(said.stderr.contains("/none/config.json: "), "{said:?}");
    let garbled = dir.path().join("garbled");
    std::fs::create_dir_all(&garbled).expect("a bundle");
    std::fs::write(garbled.join("config.json"), "{").expect("a config");
    assert_eq!(run(&garbled.display().to_string()).code, 125);
    let no_args = bundle_at(
        dir.path(),
        "no-args",
        &json!({"linux": {"cgroupsPath": "/a"}}),
    );
    assert_eq!(
        run(&no_args),
        exited(125, "crun run c1: config.json names no process.args")
    );
    let no_cgroup = bundle_at(dir.path(), "no-cgroup", &json!({"process": {"args": []}}));
    assert_eq!(
        run(&no_cgroup),
        exited(125, "crun run c1: config.json names no linux.cgroupsPath")
    );
}

/// The listener the test keeps at `events.sock`.
fn listen(place: &Place) -> UnixListener {
    UnixListener::bind(place.dir.join("events.sock")).expect("events.sock")
}

/// Runs `crun run` of a job's bundle for the action of `hash`, in a thread.
fn start(place: &Place, dir: &Path, hash: &str) -> std::thread::JoinHandle<Answer> {
    let bundle = bundle_at(dir, hash, &job_config(hash, "/agent/actions/c1/ctr"));
    let root = dir.join("root").display().to_string();
    let place = place.clone();
    std::thread::spawn(move || {
        call(
            &place,
            "crun",
            &[
                "--root", &root, "--log", "/l", "run", "--bundle", &bundle, "c1",
            ],
        )
    })
}

/// Asserts that a job's container makes the cgroup the bundle names and joins it, records
/// its process id under `--root`, tells the test its name and arguments, and then does what
/// its plan says: exits 0 with no plan, exits the code of `exit <n>`, holds until the test
/// hangs up with `hold`, and fails (125) on a plan it cannot read. Catches a container the
/// agent cannot find, cannot signal, or that ends as something the test did not ask for.
#[test]
fn a_jobs_container_reports_to_the_test_and_follows_its_plan() {
    let (dir, place) = place("stand-in-run");
    let events = listen(&place);
    std::fs::write(place.dir.join("plan").join("bb"), "exit 3\n").expect("a plan");
    std::fs::write(place.dir.join("plan").join("cc"), "hold\n").expect("a plan");
    std::fs::write(place.dir.join("plan").join("dd"), "exit x\n").expect("a plan");
    std::fs::write(place.dir.join("plan").join("ee"), "dance\n").expect("a plan");
    for (hash, code) in [("aa", 0), ("bb", 3)] {
        let running = start(&place, dir.path(), hash);
        let (stream, _) = events.accept().expect("the container connects");
        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .expect("its line");
        assert_eq!(
            line,
            format!("start\tc1\t/dev/js-init\t--\t/bin/task\t--action\t{hash}/7\t--lease\t1-2\n")
        );
        assert_eq!(running.join().expect("ran").code, code);
    }
    let cgroup = place.cgroups.join("agent/actions/c1/ctr");
    let me = std::process::id().to_string();
    assert_eq!(
        std::fs::read_to_string(cgroup.join("cgroup.procs")).expect("joined"),
        me
    );
    let pid = std::fs::read_to_string(dir.path().join("root/pid")).expect("recorded");
    assert_eq!(pid, me);

    let holding = start(&place, dir.path(), "cc");
    let (mut stream, _) = events.accept().expect("the container connects");
    let mut first = [0_u8; 6];
    stream.read_exact(&mut first).expect("its line");
    assert!(!holding.is_finished(), "it holds while the test listens");
    drop(stream);
    assert_eq!(holding.join().expect("ran"), Answer::default());

    for (hash, why) in [("dd", "plan: exit \"x\""), ("ee", "plan: \"dance\\n\"")] {
        let running = start(&place, dir.path(), hash);
        // Held until the container ends: a stream dropped at once can close before the
        // container writes its start line, which it then fails on instead of on its plan.
        let (_stream, _) = events.accept().expect("the container connects");
        assert_eq!(
            running.join().expect("ran"),
            exited(125, &format!("crun run c1: {why}"))
        );
    }
}

/// Asserts that a job's container fails (125), saying where, when it cannot make or join its
/// cgroup, record its process id, or reach the test. Catches a container that runs on
/// although the agent or the test could not see it.
#[test]
fn a_jobs_container_that_cannot_be_seen_fails() {
    let (dir, place) = place("stand-in-unseen");
    let run = |hash: &str, cgroup: &str, root: &Path| {
        let bundle = bundle_at(dir.path(), hash, &job_config(hash, cgroup));
        let root = root.display().to_string();
        call(
            &place,
            "crun",
            &["--root", &root, "run", "--bundle", &bundle, "c1"],
        )
    };
    let root = dir.path().join("root");
    std::fs::create_dir_all(&place.cgroups).expect("the mount");
    std::fs::write(place.cgroups.join("file"), "").expect("a file");
    let said = run("a1", "/file/ctr", &root);
    assert!(said.stderr.contains("/cgroup/file/ctr: "), "{said:?}");
    assert_eq!(said.code, 125);
    std::fs::create_dir_all(place.cgroups.join("busy/cgroup.procs")).expect("a directory");
    let said = run("a2", "/busy", &root);
    assert!(said.stderr.contains("joining "), "{said:?}");
    std::fs::write(dir.path().join("file-root"), "").expect("a file");
    let said = run("a4", "/ok", &dir.path().join("file-root/x"));
    assert!(said.stderr.contains("/file-root/x: "), "{said:?}");
    std::fs::create_dir_all(dir.path().join("pid-root/pid")).expect("a directory");
    let said = run("a5", "/ok", &dir.path().join("pid-root"));
    assert!(said.stderr.contains("recording the pid: "), "{said:?}");
    let said = run("a6", "/ok", &root);
    assert!(said.stderr.contains("events.sock: "), "{said:?}");
    assert_eq!(said.code, 125);
}

/// Asserts that a container dials the target of `events.sock` when it is a link (a socket's
/// path has a short limit, so the test may listen elsewhere). Catches a container that cannot
/// reach a listener whose own path is long.
#[test]
fn a_container_dials_the_listener_events_sock_links_to() {
    let (dir, place) = place("stand-in-linked");
    let listening = dir.path().join("listening.sock");
    let events = UnixListener::bind(&listening).expect("the listener");
    std::os::unix::fs::symlink(&listening, place.dir.join("events.sock")).expect("the link");
    let running = start(&place, dir.path(), "ab");
    let (stream, _) = events.accept().expect("the container connects");
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .expect("its line");
    assert!(line.starts_with("start\tc1\t"), "{line:?}");
    assert_eq!(running.join().expect("ran").code, 0);
}

/// Asserts that the programs' place is `stand-in/` beside the directory of the program, and
/// the machine's cgroup mount.
#[test]
fn the_place_is_beside_the_programs_directory() {
    assert_eq!(
        Place::of(Path::new("/node/bin/crun")),
        Place {
            dir: "/node/bin/stand-in".into(),
            cgroups: "/sys/fs/cgroup".into(),
        }
    );
    assert_eq!(Place::of(Path::new("")).dir, Path::new("/stand-in"));
}
