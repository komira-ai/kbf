//! The fakes other crates' tests stand on, checked here so a fake that drifts from the
//! host it plays fails in this crate rather than passing a kit's test it should fail.

use std::time::Duration;

use super::*;
use crate::step::Filter;

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

const UPPER: Filter = Filter {
    name: "upper",
    apply: |out| {
        if out.is_empty() {
            return Err("no output".into());
        }
        Ok(out.to_ascii_uppercase())
    },
};

/// `/usr/bin/tool` with `args`, its output filtered into `path`.
fn capture(path: &str, args: &[&str]) -> Step {
    Step::Capture {
        cmd: args
            .iter()
            .fold(Cmd::new(p("/usr/bin/tool")), |cmd, a| cmd.arg(*a)),
        filter: UPPER,
        path: p(path),
        perms: Perms::new(0o600),
        owner: None,
    }
}

/// Catches: a fake query not recorded or not answered by `answer`, or a fake capture
/// that writes the raw output (not the filter's), writes after a failed command or a
/// refusing filter, or records the file under an owner other than the runner's.
#[test]
fn the_fake_records_queries_and_writes_only_what_a_capture_filters() {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| match cmd.args.first().map(String::as_str) {
        Some("fail") => Ok(reply(1, "", "no")),
        Some("empty") => Ok(reply(0, "", "")),
        _ => Ok(reply(0, "key", "")),
    });
    let ls = Cmd::new(p("/usr/bin/ls"));
    assert_eq!(exec.query(&ls).expect("query").stdout, b"key");
    assert_eq!(*exec.queries.borrow(), [ls]);

    exec.apply(&capture("/etc/kbf/key", &[])).expect("capture");
    assert_eq!(
        exec.entries.get(&p("/etc/kbf/key")),
        Some(&Entry::File {
            perms: Perms::new(0o600),
            owner: RUNNER,
            bytes: b"KEY".to_vec(),
        })
    );
    for (arg, why) in [("fail", "exited with"), ("empty", "no output")] {
        let err = exec
            .apply(&capture("/etc/kbf/other", &[arg]))
            .expect_err(arg);
        assert!(err.to_string().contains(why), "{arg}: {err}");
        assert_eq!(exec.entries.get(&p("/etc/kbf/other")), None, "{arg}");
    }
    assert_eq!(exec.log.len(), 3);
}

/// Catches: a scratch host whose file steps or reads leave its root, which reads a
/// directory as anything but a directory, whose commands reach the real host instead of
/// `answer` (or go unrecorded), whose failed command passes, or whose root outlives it.
#[test]
fn the_scratch_host_changes_files_under_its_root_and_plays_its_commands() {
    let mut exec = ScratchHost::new("testkit-scratch-host");
    exec.answer = Box::new(|cmd| match cmd.args.first().map(String::as_str) {
        Some("fail") => Ok(reply(1, "", "no")),
        _ => Ok(reply(0, "out", "")),
    });
    let root = exec.root.clone();
    let me = Owner {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
    };
    exec.apply(&Step::Mkdir {
        path: p("/etc"),
        perms: Perms::new(0o755),
        owner: None,
    })
    .expect("mkdir");
    exec.apply(&Step::WriteFile {
        path: p("/etc/a"),
        perms: Perms::new(0o644),
        owner: None,
        bytes: b"A".to_vec(),
    })
    .expect("write");
    assert_eq!(fs_read(&exec.at("/etc/a")), b"A");
    assert_eq!(
        exec.inspect(&p("/etc")).expect("inspect"),
        Some(Entry::Dir {
            perms: Perms::new(0o755),
            owner: me,
        })
    );
    let meta = exec.stat(&p("/etc/a")).expect("stat").expect("there");
    assert_eq!((meta.kind, meta.len), (Kind::File, 1));

    let tool = Cmd::new(p("/usr/bin/tool"));
    assert_eq!(exec.query(&tool).expect("query").stdout, b"out");
    assert_eq!(*exec.queries.borrow(), std::slice::from_ref(&tool));
    exec.apply(&Step::Run(tool.clone())).expect("run");
    exec.apply(&capture("/etc/key", &[])).expect("capture");
    let err = exec
        .apply(&Step::Run(tool.arg("fail")))
        .expect_err("a non-zero exit fails");
    assert!(err.to_string().contains("exited with"), "{err}");
    let url = HttpUrl::new("http://127.0.0.1:1/readyz").expect("url");
    assert_eq!(exec.http_status(&url).expect("status"), 200);
    exec.apply(&Step::WaitHttp {
        url,
        deadline: Duration::from_secs(1),
    })
    .expect("wait");
    assert_eq!(exec.log.len(), 6);
    assert_eq!(exec.euid(), 0);
    assert!(
        exec.check_binary().is_ok(),
        "the scratch host trusts the binary"
    );
    drop(exec);
    assert!(!root.exists(), "the scratch root outlived its host");
}

fn fs_read(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).expect("read")
}

/// Catches a fake link step that replaces a file or directory (the host refuses, so a
/// kit's test on the fake would pass where the host fails) or that does not record the
/// link where a plan made after it reads it.
#[test]
fn the_fake_flips_links_and_refuses_anything_else() {
    let mut exec = FakeExec::new(0);
    let flip = |target: &str| Step::Symlink {
        path: p("/kbf/current"),
        target: target.into(),
    };
    exec.apply(&flip("releases/a")).expect("make");
    exec.apply(&flip("releases/b")).expect("flip");
    assert_eq!(
        exec.inspect(&p("/kbf/current")).expect("inspect"),
        Some(Entry::Link {
            target: "releases/b".into()
        })
    );
    let meta = exec.stat(&p("/kbf/current")).expect("stat").expect("there");
    assert_eq!(meta.kind, Kind::Other);
    let file = Entry::File {
        perms: Perms::new(0o644),
        owner: RUNNER,
        bytes: b"keep".to_vec(),
    };
    exec.entries.insert(p("/kbf/current"), file.clone());
    let err = exec.apply(&flip("releases/c")).expect_err("a file");
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(exec.entries.get(&p("/kbf/current")), Some(&file));
}

/// The child half of the test below: does nothing unless the parent set the barrier
/// directory. It takes `scratch("same-name")`, writes a file, tells the parent it is
/// ready, waits for the parent to have taken the same name, then reads the file back.
#[test]
fn scratch_child_holds_a_directory_while_the_parent_takes_the_same_name() {
    let Some(barrier) = std::env::var_os("KBF_SCRATCH_BARRIER").map(PathBuf::from) else {
        return;
    };
    let dir = scratch("same-name");
    std::fs::write(dir.join("mine"), "child").expect("write");
    std::fs::write(barrier.join("ready"), "").expect("ready");
    while !barrier.join("go").exists() {
        std::thread::yield_now();
    }
    let read = std::fs::read_to_string(dir.join("mine"));
    std::fs::write(
        barrier.join("result"),
        match read {
            Ok(text) => text,
            Err(e) => format!("error: {e}"),
        },
    )
    .expect("result");
}

/// Catches: a scratch path that two processes of one target share for the same name
/// (nextest runs each test in its own process, each counting from 0), so one's
/// `remove_dir_all` deletes the other's files. A child process holds `same-name`
/// while this process takes it; the two steps are ordered by files, not by time.
#[test]
fn two_processes_taking_one_scratch_name_do_not_share_it() {
    let barrier = scratch("barrier");
    let me = std::env::current_exe().expect("test binary");
    let mut child = std::process::Command::new(me)
        .args([
            "--exact",
            "testkit::tests::scratch_child_holds_a_directory_while_the_parent_takes_the_same_name",
            "--test-threads=1",
        ])
        .env("KBF_SCRATCH_BARRIER", &barrier)
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn the child");
    while !barrier.join("ready").exists() {
        std::thread::yield_now();
    }
    let mine = scratch("same-name");
    std::fs::write(barrier.join("go"), "").expect("go");
    assert!(child.wait().expect("child").success());
    assert_eq!(
        std::fs::read_to_string(barrier.join("result")).expect("the child's result"),
        "child"
    );
    assert!(mine.read_dir().expect("mine").next().is_none());
}
