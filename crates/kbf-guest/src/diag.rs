//! `probe-diag`: what a root process in the guest sees of the mounted share and which
//! file operation on it is refused.
//!
//! The second E1 run mounted the share and ran `kbf-guest` from it, but every write
//! into `results/` failed with `Operation not permitted`, as did a root `umount`. This
//! mode reads the identity, the mount table, the owner, mode and flags of the mount point
//! and of `results/`, and then tries each step of a write (create, write, rename, make a
//! directory, remove) in the share root, in `results/` and in a control directory on the
//! guest's own disk, reporting each step's errno. It prints one JSON object on stdout
//! (the launch daemon sends that to a file the harvest reads) and also tries to leave it
//! in `results/kbf-diag.json`. It also says whether the guest can connect to the host on
//! virtio-socket and whether a test frame came back acknowledged. It runs system tools for the facts and does no shell work.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::probe::{TICK, WAIT};

/// `probe-diag`'s arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct DiagArgs {
    /// The mounted share's root.
    #[arg(long, default_value = "/private/var/kbf-mnt")]
    pub share: PathBuf,
    /// Wait for this path to exist first (up to the probe wait); the diagnosis is made
    /// even if it never appears.
    #[arg(long)]
    pub wait_path: Option<PathBuf>,
    /// A directory on the guest's own disk, written the same way as a control.
    #[arg(long, default_value = "/private/var/tmp")]
    pub control_dir: PathBuf,
    /// The system's own automount of the share, written to like the others.
    #[arg(long, default_value = "/Volumes/My Shared Files/kbf")]
    pub automount_dir: PathBuf,
    /// A second virtiofs tag to mount, write into and unmount (root's `umount` was refused).
    #[arg(long, default_value = "kbfprobe")]
    pub tag: String,
    /// Where to mount it.
    #[arg(long, default_value = "/private/tmp/kbfmnt-diag")]
    pub mountpoint: PathBuf,
    /// The vsock port the host listens on for results; the diagnosis sends it a test frame.
    #[arg(long, default_value_t = 5000)]
    pub vsock_port: u32,
}

/// What the diagnosis asks of the guest; a fake in tests.
pub trait DiagHost {
    /// Runs the system tool `program` with `args`; its exit code, stdout and stderr.
    fn run(&self, program: &str, args: &[&str]) -> Value;
    /// The real and effective user ids and group ids.
    fn ids(&self) -> Value;
    fn exists(&self, path: &Path) -> bool;
    fn pause(&self, d: Duration);
    /// Connects to the host on vsock `port`, sends a `log` frame named `vsock-test` and
    /// waits for the acknowledgement: whether the connection was made, whether the frame
    /// came back acknowledged, and the errno or error of the step that failed.
    fn vsock_round_trip(&self, port: u32) -> Value;
    /// Mounts `tag` at `mountpoint`, runs `inside` on it, and unmounts it again with the
    /// system call; the errno of each of the three.
    fn mount_cycle(&self, tag: &str, mountpoint: &Path, inside: &dyn Fn(&Path) -> Value) -> Value;
}

/// The tools whose output is the facts: who the process is, what is mounted, and the
/// owner, mode, flags, extended attributes and ACL of the paths. Each is `(label,
/// program, arguments)`.
fn facts(a: &DiagArgs) -> Vec<(String, &'static str, Vec<String>)> {
    let mut v = vec![
        ("id".to_owned(), "/usr/bin/id", Vec::new()),
        ("mount".to_owned(), "/sbin/mount", Vec::new()),
        ("sw_vers".to_owned(), "/usr/bin/sw_vers", Vec::new()),
        (
            "csrutil".to_owned(),
            "/usr/bin/csrutil",
            vec!["status".to_owned()],
        ),
    ];
    for (label, path) in [
        ("mountpoint", a.share.clone()),
        ("results", a.share.join("results")),
        ("control", a.control_dir.clone()),
    ] {
        v.push((
            format!("ls {label}"),
            "/bin/ls",
            vec!["-ldeO@".to_owned(), path.to_string_lossy().into_owned()],
        ));
        v.push((
            format!("stat {label}"),
            "/usr/bin/stat",
            vec![
                "-f".to_owned(),
                "uid=%u gid=%g mode=%Sp flags=%Sf dev=%d".to_owned(),
                path.to_string_lossy().into_owned(),
            ],
        ));
    }
    v
}

/// Tries each step of a write inside `dir` and reports every errno. The files it makes
/// are removed again, and a failed removal is reported too.
pub fn write_steps(dir: &Path) -> Value {
    let file = dir.join("kbf-diag-file");
    let moved = dir.join("kbf-diag-moved");
    let sub = dir.join("kbf-diag-dir");
    let step = |name: &str, r: std::io::Result<()>| match r {
        Ok(()) => json!({"step": name, "ok": true}),
        Err(e) => {
            json!({"step": name, "ok": false, "errno": e.raw_os_error(), "error": e.to_string()})
        }
    };
    let created = step("create", std::fs::File::create(&file).map(drop));
    let wrote = step(
        "write",
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .and_then(|mut f| f.write_all(b"kbf-diag\n")),
    );
    let renamed = std::fs::rename(&file, &moved);
    let left = if renamed.is_ok() { &moved } else { &file };
    let renamed = step("rename", renamed);
    let made = step("mkdir", std::fs::create_dir(&sub));
    let mut steps = vec![
        created,
        wrote,
        renamed,
        made,
        step("remove_file", std::fs::remove_file(left)),
        step("remove_dir", std::fs::remove_dir(&sub)),
    ];
    // A later step can fail only because an earlier one did (nothing to remove); the
    // first failed step is the one that matters.
    let first = steps.iter().position(|s| s["ok"] == json!(false));
    steps.push(json!({"first_failed_step": first.map(|i| steps[i]["step"].clone())}));
    json!({"dir": dir, "steps": steps})
}

/// Waits for `wait_path` (when given), then gathers the facts and tries the writes.
pub fn diagnose(host: &dyn DiagHost, a: &DiagArgs) -> Value {
    let mut waited = Duration::ZERO;
    let mut appeared = true;
    if let Some(p) = &a.wait_path {
        while !host.exists(p) {
            if waited >= WAIT {
                appeared = false;
                break;
            }
            host.pause(TICK);
            waited += TICK;
        }
    }
    let facts: Value = facts(a)
        .iter()
        .map(|(label, program, args)| {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            (label.clone(), host.run(program, &args))
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    json!({
        "wait_path": a.wait_path,
        "wait_path_appeared": appeared,
        "waited_s": waited.as_secs_f64(),
        "ids": host.ids(),
        "vsock": host.vsock_round_trip(a.vsock_port),
        "facts": facts,
        "writes": [
            write_steps(&a.share),
            write_steps(&a.share.join("results")),
            write_steps(&a.control_dir),
            write_steps(&a.automount_dir),
        ],
        "second_mount": host.mount_cycle(&a.tag, &a.mountpoint, &write_steps),
        "matrix": matrix(host, &a.share),
    })
}

/// Where a matrix share is mounted.
fn matrix_mountpoint(name: &str) -> PathBuf {
    Path::new("/private/tmp").join(format!("kbfmx-{name}"))
}

/// Mounts every share the host's manifest (`matrix.json` in the main share) lists, tries
/// the write steps in it (in its sub directory for a multiple-directory share), and
/// unmounts it. One row per share, with the host's description of what differs.
fn matrix(host: &dyn DiagHost, share: &Path) -> Value {
    let path = share.join("matrix.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return json!({"manifest": path, "error": e.to_string()}),
    };
    let rows = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(rows)) => rows,
        Ok(other) => return json!({"manifest": path, "error": format!("not a list: {other}")}),
        Err(e) => return json!({"manifest": path, "error": format!("not JSON: {e}")}),
    };
    Value::Array(
        rows.iter()
            .map(|row| {
                let text = |k: &str| row[k].as_str().unwrap_or_default().to_owned();
                let (name, tag) = (text("name"), text("tag"));
                let sub = row["subdir"].as_str().map(str::to_owned);
                let inside = |mp: &Path| {
                    let dir = sub
                        .as_ref()
                        .map_or_else(|| mp.to_path_buf(), |s| mp.join(s));
                    write_steps(&dir)
                };
                json!({
                    "name": name,
                    "factor": row["factor"],
                    "tag": tag,
                    "subdir": sub,
                    "read_only": row["read_only"],
                    "host_mode": row["host_mode"],
                    "cycle": host.mount_cycle(&tag, &matrix_mountpoint(&name), &inside),
                })
            })
            .collect(),
    )
}

/// Prints `report` on stdout and tries to leave it in the share's `results/` too (a
/// failure there is part of the answer, so it is printed, not returned).
pub fn emit(report: &Value, share: &Path, out: &mut dyn Write) {
    let text = format!("{report:#}");
    let _ = writeln!(out, "{text}");
    let dest = share.join("results").join("kbf-diag.json");
    if let Err(e) = std::fs::write(&dest, &text) {
        let _ = writeln!(out, "kbf-diag: write {}: {e}", dest.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct Fake {
        runs: RefCell<Vec<String>>,
        exists_after: Cell<u32>,
        pauses: Cell<u32>,
        mounted: RefCell<Vec<String>>,
        root: PathBuf,
    }

    impl Fake {
        fn new(exists_after: u32, root: &Path) -> Self {
            Self {
                root: root.to_owned(),
                runs: RefCell::new(Vec::new()),
                exists_after: Cell::new(exists_after),
                pauses: Cell::new(0),
                mounted: RefCell::new(Vec::new()),
            }
        }
    }

    impl DiagHost for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Value {
            self.runs
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            json!({"exit_code": 0, "stdout": program})
        }
        fn ids(&self) -> Value {
            json!({"uid": 0})
        }
        fn exists(&self, _: &Path) -> bool {
            self.pauses.get() >= self.exists_after.get()
        }
        fn pause(&self, _: Duration) {
            self.pauses.set(self.pauses.get() + 1);
        }
        fn vsock_round_trip(&self, port: u32) -> Value {
            json!({"port": port, "connected": true, "round_trip": true})
        }
        fn mount_cycle(&self, tag: &str, mp: &Path, inside: &dyn Fn(&Path) -> Value) -> Value {
            self.mounted
                .borrow_mut()
                .push(format!("{tag} {}", mp.display()));
            // A matrix share is mounted under /private/tmp; the fake keeps it under its root.
            let real = if mp.starts_with("/private/tmp") {
                let r = self.root.join(mp.file_name().expect("leaf"));
                std::fs::create_dir_all(&r).expect("mount dir");
                r
            } else {
                mp.to_path_buf()
            };
            json!({"inside": inside(&real)})
        }
    }

    fn args(dir: &Path) -> DiagArgs {
        std::fs::create_dir_all(dir.join("share/results")).expect("results");
        std::fs::create_dir_all(dir.join("control")).expect("control");
        DiagArgs {
            share: dir.join("share"),
            wait_path: Some(dir.join("share/results")),
            control_dir: dir.join("control"),
            automount_dir: dir.join("automount"),
            tag: "t".into(),
            mountpoint: dir.join("mp"),
            vsock_port: 4242,
        }
    }

    fn tmp(name: &str) -> kbf_testkit::dir::TestDir {
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), &format!("kbf-diag-{name}"))
    }

    /// Catches a diagnosis that leaves out a fact the EPERM question needs (the user, the
    /// mount table entry, the owner/mode/flags of the mount point and of `results/`), asks
    /// a shell for them, drops a write target, or forgets the second mount.
    #[test]
    fn the_diagnosis_asks_for_every_fact_and_tries_every_target() {
        let d = tmp("facts");
        let a = args(&d);
        let host = Fake::new(0, &d);
        let r = diagnose(&host, &a);
        let runs = host.runs.borrow().join("\n");
        for needle in [
            "/usr/bin/id",
            "/sbin/mount",
            "/bin/ls -ldeO@",
            "/usr/bin/stat -f",
            "csrutil status",
        ] {
            assert!(runs.contains(needle), "{needle}: {runs}");
        }
        assert!(runs.contains(&format!("{}", a.share.display())), "{runs}");
        assert!(
            runs.contains(&format!("{}", a.share.join("results").display())),
            "{runs}"
        );
        // Only these system tools run, each by absolute path.
        let tools = [
            "/usr/bin/id",
            "/sbin/mount",
            "/usr/bin/sw_vers",
            "/usr/bin/csrutil",
            "/bin/ls",
            "/usr/bin/stat",
        ];
        for line in runs.lines() {
            let program = line.split(' ').next().expect("program");
            assert!(tools.contains(&program), "{line}");
        }
        let dirs: Vec<String> = r["writes"]
            .as_array()
            .expect("writes")
            .iter()
            .map(|w| w["dir"].as_str().expect("dir").to_owned())
            .collect();
        assert_eq!(
            dirs,
            [
                a.share.display().to_string(),
                a.share.join("results").display().to_string(),
                a.control_dir.display().to_string(),
                a.automount_dir.display().to_string()
            ]
        );
        assert_eq!(r["ids"]["uid"], 0);
        // The vsock question is asked on the port the daemon was given, and its answer is
        // in the report next to the share facts.
        assert_eq!(r["vsock"]["port"], 4242);
        assert_eq!(r["vsock"]["round_trip"], true);
        assert_eq!(r["facts"]["mount"]["stdout"], "/sbin/mount");
        assert_eq!(
            *host.mounted.borrow(),
            [format!("t {}", a.mountpoint.display())]
        );
        assert_eq!(
            r["second_mount"]["inside"]["dir"],
            a.mountpoint.to_string_lossy().as_ref()
        );
        assert_eq!(r["wait_path_appeared"], true);
    }

    /// Catches a wait that is not bounded, one that returns before the path appears, and
    /// a missing mount that stops the diagnosis (the facts are the point then).
    #[test]
    fn the_wait_is_bounded_and_the_diagnosis_is_made_either_way() {
        let d = tmp("wait");
        let a = args(&d);
        let host = Fake::new(3, &d);
        let r = diagnose(&host, &a);
        assert_eq!((host.pauses.get(), r["waited_s"].clone()), (3, json!(1.5)));
        // Nothing to wait for: no pause, and the path is reported as not asked for.
        let mut none = args(&d);
        none.wait_path = None;
        let idle = Fake::new(u32::MAX, &d);
        let r = diagnose(&idle, &none);
        assert_eq!(
            (idle.pauses.get(), r["wait_path_appeared"].clone()),
            (0, json!(true))
        );
        assert_eq!(r["wait_path"], json!(null));
        let never = Fake::new(u32::MAX, &d);
        let r = diagnose(&never, &a);
        assert_eq!(r["wait_path_appeared"], false);
        assert!(never.pauses.get() > 100);
        assert!(never.pauses.get() < 1000);
        assert!(!never.runs.borrow().is_empty());
    }

    /// Catches write steps that hide which one failed, report success on a directory that
    /// refuses writes, or leave their files behind.
    #[test]
    fn the_write_steps_report_the_first_refused_one_and_clean_up() {
        let d = tmp("steps");
        let ok = write_steps(&d);
        assert!(
            ok["steps"]
                .as_array()
                .expect("steps")
                .iter()
                .take(6)
                .all(|s| s["ok"] == true),
            "{ok}"
        );
        assert_eq!(ok["steps"][6]["first_failed_step"], json!(null));
        assert_eq!(std::fs::read_dir(&d).expect("dir").count(), 0, "cleaned up");
        let missing = write_steps(&d.join("no-such-dir"));
        assert_eq!(missing["steps"][0]["step"], "create");
        assert_eq!(missing["steps"][0]["ok"], false);
        assert_eq!(missing["steps"][0]["errno"], 2);
        assert_eq!(missing["steps"][6]["first_failed_step"], "create");
        // A file where the sub directory goes: only mkdir (and its removal) fail.
        std::fs::write(d.join("kbf-diag-dir"), "x").expect("blocker");
        let r = write_steps(&d);
        assert_eq!(r["steps"][6]["first_failed_step"], "mkdir");
    }

    /// Catches a matrix that skips a share the host listed, mounts the wrong tag, writes in
    /// the wrong directory of a multiple-directory share, loses the host's description, or
    /// fails the whole diagnosis on a missing or broken manifest.
    #[test]
    fn every_share_in_the_manifest_is_mounted_and_written_to() {
        let d = tmp("matrix");
        let a = args(&d);
        std::fs::write(
            a.share.join("matrix.json"),
            r#"[{"name":"base","tag":"kbfmx0","factor":"none","subdir":null,"read_only":false,"host_mode":"777"},
                {"name":"multiple","tag":"kbfmx4","factor":"multi","subdir":"m","read_only":false,"host_mode":"777"}]"#,
        )
        .expect("manifest");
        let multiple = d.join("kbfmx-multiple");
        std::fs::create_dir_all(multiple.join("m")).expect("sub");
        let host = Fake::new(0, &d);
        let r = diagnose(&host, &a);
        let rows = r["matrix"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "base");
        assert_eq!(rows[0]["factor"], "none");
        assert_eq!(rows[0]["host_mode"], "777");
        let mounted = host.mounted.borrow();
        assert!(
            mounted.contains(&"kbfmx0 /private/tmp/kbfmx-base".to_owned()),
            "{mounted:?}"
        );
        assert!(
            mounted.contains(&"kbfmx4 /private/tmp/kbfmx-multiple".to_owned()),
            "{mounted:?}"
        );
        drop(mounted);
        // The plain share is written at its mount point, the multiple one in its sub directory.
        assert_eq!(
            rows[0]["cycle"]["inside"]["dir"],
            d.join("kbfmx-base").to_string_lossy().as_ref()
        );
        assert_eq!(
            rows[1]["cycle"]["inside"]["dir"],
            multiple.join("m").to_string_lossy().as_ref()
        );
        assert_eq!(rows[1]["subdir"], "m");
        assert_eq!(rows[1]["cycle"]["inside"]["steps"][0]["ok"], true);
        // No manifest, a manifest that is not JSON, and one that is not a list.
        std::fs::remove_file(a.share.join("matrix.json")).expect("rm");
        assert!(diagnose(&host, &a)["matrix"]["error"].is_string());
        std::fs::write(a.share.join("matrix.json"), "{").expect("bad");
        assert!(
            diagnose(&host, &a)["matrix"]["error"]
                .as_str()
                .expect("e")
                .contains("not JSON")
        );
        std::fs::write(a.share.join("matrix.json"), "{}").expect("obj");
        assert!(
            diagnose(&host, &a)["matrix"]["error"]
                .as_str()
                .expect("e")
                .contains("not a list")
        );
        // A row with fields missing is still a row (empty strings), not a panic.
        std::fs::write(a.share.join("matrix.json"), "[{}]").expect("empty row");
        assert_eq!(diagnose(&host, &a)["matrix"][0]["name"], "");
    }

    /// Catches a report that is not printed, one that is not left in `results/`, and a
    /// refused copy into the share that is not itself reported.
    #[test]
    fn the_report_is_printed_and_left_in_the_share() {
        let d = tmp("emit");
        let a = args(&d);
        let mut out = Vec::new();
        emit(&json!({"k": 1}), &a.share, &mut out);
        assert!(String::from_utf8_lossy(&out).contains("\"k\": 1"));
        assert!(
            std::fs::read_to_string(a.share.join("results/kbf-diag.json"))
                .expect("copy")
                .contains("\"k\"")
        );
        let mut out = Vec::new();
        emit(&json!({}), &d.join("no-share"), &mut out);
        assert!(
            String::from_utf8_lossy(&out).contains("kbf-diag: write"),
            "{out:?}"
        );
    }
}
