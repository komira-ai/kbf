//! The two stand-in `podman`s (`support/fake.rs`) against each other: the same calls,
//! in the same directory, must give the same exit codes, the same output and the same
//! files, byte for byte, so the driver suites mean the same under either.

mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The action both `start`s run: its environment, whether it leads its own process
/// group and session (fields 5 and 6 of its `/proc` stat), both streams and an exit
/// code of its own.
const ACTION: &str = r#"echo "CG=$CG ROOT=$ROOT UPPER=$UPPER FAKE_LEASE=$FAKE_LEASE"
read -r -a stat < /proc/$$/stat
echo "leads group $(( stat[4] == $$ )) session $(( stat[5] == $$ ))"
echo err >&2
exit 3"#;

/// One call: files below the fake's directory to write (or, with `None`, remove) first, then the arguments after
/// `--cgroup-manager=cgroupfs` (`None`: the flag is left out too).
struct Call {
    knobs: &'static [(&'static str, Option<&'static str>)],
    args: Option<Vec<String>>,
}

fn call(knobs: &'static [(&'static str, Option<&'static str>)], args: &[&str]) -> Call {
    Call {
        knobs,
        args: Some(args.iter().map(|a| (*a).to_owned()).collect()),
    }
}

/// What one call gave.
#[derive(Debug, PartialEq)]
struct Answer {
    args: Option<Vec<String>>,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// A file or directory the calls left: its mode and, for a file, its bytes.
#[derive(Debug, PartialEq)]
enum Entry {
    Dir(u32),
    File(u32, String),
    Link(PathBuf),
}

/// Each verb, and each knob that changes one but `image-hangs` (a 30-second sleep).
/// None of them makes a program other than the fake fail (its message would be that
/// program's, not the fake's).
fn calls(dir: &Path) -> Vec<Call> {
    let root = dir.join("lease/root");
    let upper = dir.join("lease/upper");
    let volume = format!(
        "--volume={}:/w:O,upperdir={},workdir={}",
        root.display(),
        upper.display(),
        dir.join("lease/work").display()
    );
    let upper = upper.to_str().expect("utf-8 path").to_owned();
    let gone = dir.join("gone");
    let gone = gone.to_str().expect("utf-8 path").to_owned();
    let create = [
        "create",
        "--pull=never",
        "--name=kbf-lease-1-1",
        "--label=kbf.owner=node-1",
        "--cgroup-parent=/actions/kbf-lease-1-1",
        &volume,
        "docker://registry.test/busybox",
    ];
    let ps = [
        "ps",
        "--all",
        "--filter=label=kbf.owner=node-1",
        "--format=x",
    ];
    vec![
        Call {
            knobs: &[],
            args: None,
        },
        call(&[], &["frobnicate"]),
        call(&[], &["info", "--format=x"]),
        call(&[("state/info-fails", Some(""))], &["info"]),
        call(&[], &["image", "inspect", "--format={{.Id}}", "ref"]),
        call(&[("state/image-id", None)], &["image", "inspect", "ref"]),
        call(&[("state/create-fails", Some(""))], &create),
        call(&[("state/create-fails", None)], &["create"]),
        call(&[("state/block-log", Some("stdout"))], &create),
        call(&[("state/block-log", None)], &ps),
        call(&[("state/ps-extra", Some("node-1 other\n"))], &ps),
        call(&[("state/ps-fails", Some(""))], &ps),
        call(&[], &["inspect", "--type=container", "kbf-lease-1-1"]),
        call(&[], &["kill", "--signal=TERM", "kbf-lease-1-1"]),
        call(
            &[("state/action.sh", Some(ACTION))],
            &["start", "--attach", "kbf-lease-1-1"],
        ),
        call(&[], &["inspect", "kbf-lease-1-1"]),
        call(&[("state/inspect-fails", Some(""))], &["inspect", "x"]),
        call(
            &[("state/kill-fails", Some(""))],
            &["kill", "--signal=TERM", "x"],
        ),
        call(&[], &["unshare", "chown", "-hR", "1:1", "--", &upper, "/b"]),
        call(
            &[("state/status-override", Some("stopped 0\n"))],
            &["start", "--attach", "kbf-lease-1-1"],
        ),
        call(&[], &["unshare", "chown", "-hR", "0:0", "--", &upper]),
        call(
            &[("state/chown-fails", Some("1:1"))],
            &["unshare", "chown", "-hR", "1:1", "--"],
        ),
        call(&[], &["unshare", "chown", "-R", "1:1", "--", "/a"]),
        call(
            &[("state/rm-fails", Some(""))],
            &["rm", "--force", "kbf-lease-1-1"],
        ),
        call(
            &[
                ("state/rm-fails", None),
                // No action to kill: the pid of one that has ended may be another's now.
                ("state/pid", None),
                ("cgroup/actions/kbf-lease-1-1/cgroup.kill", Some("1\n")),
            ],
            &["rm", "--force", "kbf-lease-1-1"],
        ),
        call(
            &[("state/unshare-fails", Some(""))],
            &["unshare", "rm", "-rf", "--", &gone],
        ),
        call(
            &[
                ("state/unshare-fails", None),
                ("state/unshare-noop", Some("")),
            ],
            &["unshare", "rm", "-rf", "--", &gone],
        ),
        call(
            &[("state/unshare-noop", None)],
            &["unshare", "rm", "-rf", "--", &gone],
        ),
        call(&[("state/vanish-after-create", Some(""))], &create),
    ]
}

/// Lays out a fake's directory at `dir` with `program` as its `podman`, makes the
/// calls, and returns their answers and what they left.
fn run(dir: &Path, program: &Path) -> (Vec<Answer>, BTreeMap<PathBuf, Entry>) {
    let state = dir.join("state");
    for d in [
        &state,
        &dir.join("cgroup/actions/kbf-lease-1-1"),
        &dir.join("lease/root"),
    ] {
        std::fs::create_dir_all(d).expect("mkdir");
    }
    std::fs::create_dir_all(dir.join("lease/upper/sub")).expect("mkdir upper");
    std::fs::set_permissions(
        dir.join("lease/upper"),
        std::fs::Permissions::from_mode(0o751),
    )
    .expect("chmod upper");
    std::fs::write(dir.join("cgroup/actions/kbf-lease-1-1/memory.max"), "max\n").expect("max");
    // A tree `unshare rm` can delete only once it has given itself the modes back.
    std::fs::create_dir_all(dir.join("gone/locked")).expect("mkdir gone");
    std::fs::write(dir.join("gone/locked/f"), "x").expect("write gone");
    std::fs::set_permissions(
        dir.join("gone/locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("chmod gone");
    std::fs::write(state.join("image-id"), "img1\n").expect("image id");
    std::fs::write(state.join("nonce"), "n:").expect("nonce");
    let link = dir.join("podman");
    std::os::unix::fs::symlink(program, &link).expect("link podman");
    let mut answers = Vec::new();
    for call in calls(dir) {
        for (name, contents) in call.knobs {
            match contents {
                Some(contents) => std::fs::write(dir.join(name), contents).expect("knob"),
                None => {
                    let _ = std::fs::remove_file(dir.join(name));
                }
            }
        }
        let mut command = Command::new(&link);
        if let Some(args) = &call.args {
            command.arg("--cgroup-manager=cgroupfs").args(args);
        }
        let output = command.output().expect("run the fake");
        answers.push(Answer {
            args: call.args,
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    (answers, tree(dir))
}

/// Every entry below `dir` by relative path. The pids `start` records differ from run
/// to run, so only their presence is kept.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).expect("list").flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).expect("stat");
            let mode = meta.permissions().mode() & 0o7777;
            let name = path.strip_prefix(dir).expect("below dir").to_owned();
            let entry = if meta.is_symlink() {
                Entry::Link(std::fs::read_link(&path).expect("readlink"))
            } else if meta.is_dir() {
                if mode & 0o500 == 0o500 {
                    pending.push(path);
                }
                Entry::Dir(mode)
            } else {
                let bytes = std::fs::read(&path).unwrap_or_default();
                let text = match name.file_name().and_then(|n| n.to_str()) {
                    Some("pid" | "start-pid") => "<a pid>".to_owned(),
                    _ => String::from_utf8_lossy(&bytes).into_owned(),
                };
                Entry::File(mode, text)
            };
            entries.insert(name, entry);
        }
    }
    entries
}

/// Catches the Rust fake answering a call, or writing a record, otherwise than the
/// script: an exit code, a line of output, a record's format (`calls`, `events`,
/// `status`, `create.args`, ...), a knob it ignores or a mode it leaves differently.
/// Both run in the same directory, one after the other, so the paths in their records
/// are the same.
#[test]
fn the_rust_fake_answers_and_records_as_the_script_does() {
    let base = support::scratch("fake-parity");
    let dir = base.join("fake");
    let rust = PathBuf::from(env!("CARGO_BIN_EXE_kbf-fake-podman"));
    let mut runs = Vec::new();
    for (name, program) in [("script", support::fake::script()), ("rust", rust)] {
        let (answers, mut files) = run(&dir, &program);
        // The link differs by design, and is the one entry that may.
        let link = files.remove(Path::new("podman"));
        assert!(
            link.is_none(),
            "{name}: vanish-after-create left the podman link"
        );
        support::force_remove(&dir);
        runs.push((name, answers, files));
    }
    let (_, script_answers, script_files) = &runs[0];
    let (_, rust_answers, rust_files) = &runs[1];
    // A control: the calls reached the knobs and records they are there for.
    assert!(
        script_answers
            .iter()
            .any(|a| a.code == Some(3) && a.stdout.contains("leads group 1 session 1")),
        "no start ran the action in a session of its own: {script_answers:#?}"
    );
    for name in [
        "calls",
        "events",
        "status",
        "create.args",
        "ps.args",
        "owner",
        "removed",
        "killed-before-rm",
    ] {
        assert!(
            script_files.contains_key(&Path::new("state").join(name)),
            "the script never wrote {name}"
        );
    }
    assert!(
        !script_files.contains_key(Path::new("gone")),
        "unshare rm never ran"
    );
    for (want, got) in script_answers.iter().zip(rust_answers) {
        assert_eq!(got, want, "the two fakes answer a call differently");
    }
    assert_eq!(rust_answers.len(), script_answers.len());
    assert_eq!(
        rust_files, script_files,
        "the two fakes leave different files"
    );
    support::force_remove(&base);
}
