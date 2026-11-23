//! The start of a Mac job's Command: a run whose root and private directory are named
//! relatively, as a Mac job's `.` is, and the cache `xcrun` keeps.

use std::path::PathBuf;

use super::*;

/// `path`, an absolute path, named relative to this process's current directory (`..` up to `/`,
/// then the rest), so a test runs with a relative root without changing directory.
fn relative_to_here(path: &Path) -> PathBuf {
    let here = std::fs::canonicalize(".").expect("cwd");
    let path = std::fs::canonicalize(path).expect("path");
    let mut relative = PathBuf::new();
    for _ in here.components().skip(1) {
        relative.push("..");
    }
    relative.push(path.strip_prefix("/").expect("absolute"));
    relative
}

impl Rig {
    /// [`Rig::run_job`], with the root and the private directory named relatively.
    async fn run_job_relative(&self, command: &Command) -> Result<Ran, RunError> {
        let root = relative_to_here(&self.root());
        // The private directory is made by the run; its parent is the rig's own.
        let private = relative_to_here(&self.dir).join("private");
        run(
            command,
            &Run {
                root: &root,
                private: &private,
                stdout: &self.stdout,
                stderr: &self.stderr,
                deadline: LONG,
                network: NETWORK,
                unreadable: &[],
                hidden: std::slice::from_ref(&root),
                relay: Some(Path::new("/no/such/kbf-task")),
                job: Some(Job::Group),
            },
        )
        .await
    }
}

/// A job whose root is named relatively still runs a program named by a relative path under its
/// work directory: the program is not looked up a second time below the directory the Command
/// starts in. Catches a work directory left relative, which on macOS starts `./work/bin/pwd` in
/// `work` as `work/work/bin/pwd` (No such file or directory, exit 11 in a Mac job).
#[tokio::test]
async fn a_job_whose_root_is_relative_runs_the_program_under_its_work_directory() {
    let rig = Rig::new("job-relative-root");
    let bin = rig.root().join("work/bin");
    std::fs::create_dir_all(&bin).expect("bin");
    // A link, not a copy: a copied system binary can be ended by macOS before it runs.
    std::os::unix::fs::symlink("/bin/pwd", bin.join("pwd")).expect("tool");
    let mut command = program(&["./bin/pwd"]);
    command.working_directory = "work".into();
    let ran = rig.run_job_relative(&command).await.expect("ran");
    assert_eq!(ran.exit_code, 0);
    let work = std::fs::canonicalize(rig.root().join("work")).expect("work");
    assert_eq!(rig.out(), format!("{}\n", work.display()));
}

/// A job whose root and private directory are named relatively gives its Command each private
/// variable as an absolute path, the directory the run made. Catches relative values, which
/// name other paths once the Command is in its work directory (`xcrun` refuses a relative
/// `xcrun_db`: "environment variable 'xcrun_db' is not absolute, ignoring").
#[tokio::test]
async fn a_job_whose_root_is_relative_names_its_private_directories_absolutely() {
    let rig = Rig::new("job-relative-private");
    let ran = rig
        .run_job_relative(&program(&["/usr/bin/env"]))
        .await
        .expect("ran");
    assert_eq!(ran.exit_code, 0);
    let out = rig.out();
    let env: std::collections::HashMap<&str, &str> =
        out.lines().filter_map(|l| l.split_once('=')).collect();
    let private = std::fs::canonicalize(rig.private()).expect("private");
    for (name, sub) in PRIVATE_VARS.iter().chain(PLATFORM_VARS) {
        let value = Path::new(env.get(name).copied().unwrap_or_default());
        assert!(value.is_absolute(), "{name}={}", value.display());
        assert_eq!(
            std::fs::canonicalize(value.parent().expect("parent"))
                .expect("made")
                .join(value.file_name().expect("name")),
            private.join(sub),
            "{name}"
        );
    }
}

/// On macOS the Command gets `xcrun_db`, a file in its own private `tmp` (so `xcrun` and the
/// tools behind its shims keep their cache where the run may write); on Linux it gets no such
/// variable. Catches: a Mac run whose `xcrun` falls back to the user's `/var/folders`, which a
/// job may not write (`ld -v` then prints an error, and a host identity made from it is wrong).
#[tokio::test]
async fn on_a_mac_xcrun_keeps_its_cache_in_the_runs_private_tmp() {
    let rig = Rig::new("xcrun-db");
    let env = env_of(&rig, &program(&["/usr/bin/env"])).await;
    let tmp = rig.private().join("tmp");
    if cfg!(target_os = "macos") {
        assert_eq!(
            env.get("xcrun_db").map(PathBuf::from),
            Some(tmp.join("xcrun_db"))
        );
        assert!(tmp.is_dir());
    } else {
        assert_eq!(env.get("xcrun_db"), None);
    }
}
