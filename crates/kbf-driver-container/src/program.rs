//! A program the container could not start: the action's own failure, not the farm's.
//!
//! A container's files are its image, fixed by digest, and the action's input root, and
//! its environment is the `Command`'s alone. A program the OCI runtime cannot find or
//! execute there would be the same on every node, so it is the `Command`'s fault
//! (docs/design/failure-classes.md, 6.7): answered as the action's result, exit 127
//! (not found) or 126 (found, not an executable file), as a shell and `podman run`
//! answer, with kbf's message on stderr before Podman's own words. A non-zero exit is
//! never cached.
//!
//! Only crun's own report of its program lookup counts (crun 1.14, under the nodes'
//! Podman 4.9): ``executable file `<argv[0]>` not found in $PATH`` for any `ENOENT`,
//! and `open executable: ` for a file it may not execute or that is not a regular
//! file. Podman's wrapping does not count: it calls every runtime "No such file or
//! directory" "a command that was not found" (a missing mount source among them) and
//! every "Operation not permitted" "OCI permission denied" (a cgroup it may not write),
//! which are the farm's. Neither does a report from any other runtime: that start
//! stays the farm's failure, with Podman's words in it.

use std::path::{Path, PathBuf};

use kbf_daemon::RuntimeError;

use crate::podman::{ContainerSpec, EXEC_ROOT};
use crate::runtime::failed;

/// How much of the runtime's words are read back when the container did not run.
const SAID_MAX: u64 = 64 << 10;

/// Why the container could not start the action's program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unstartable {
    NotFound,
    NotExecutable,
}

impl Unstartable {
    /// What the action answers: the shell's and `podman run`'s exit codes.
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::NotFound => 127,
            Self::NotExecutable => 126,
        }
    }
}

/// What crun's report in `said` (`podman start`'s stderr, for a container that did not
/// run) says of `program`, if it is a report of the program lookup.
pub(crate) fn classify(program: &str, said: &str) -> Option<Unstartable> {
    if said.contains(&format!("executable file `{program}` not found in $PATH")) {
        Some(Unstartable::NotFound)
    } else if said.contains("crun: open executable: ") {
        Some(Unstartable::NotExecutable)
    } else {
        None
    }
}

/// kbf's line for the action's stderr: the program, and where it was looked up.
pub(crate) fn message(
    why: Unstartable,
    program: &str,
    env: &[(String, String)],
    working_directory: &str,
) -> String {
    let what = match why {
        Unstartable::NotFound => "was not found",
        Unstartable::NotExecutable => "is not an executable file",
    };
    let path = env.iter().find(|(name, _)| name == "PATH").map(|(_, v)| v);
    let place = match (program.contains('/'), path) {
        (true, _) => {
            let dir = if working_directory.is_empty() {
                EXEC_ROOT.to_owned()
            } else {
                format!("{EXEC_ROOT}/{working_directory}")
            };
            format!("in the container (image and input root, working directory {dir})")
        }
        (false, Some(path)) => format!("on the Command's PATH {path:?}"),
        (false, None) => "on the Command's PATH: a name without a slash is looked up there, \
            and the Command sets no PATH (an action gets only the Command's environment)"
            .to_owned(),
    };
    format!("kbf: the action's program `{program}` {what} {place}")
}

/// Where a runtime's own words about a container that did not run are, and what they are
/// called in the messages built from them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Said {
    /// The file the words are read from.
    source: PathBuf,
    /// The action's stderr file, which a lookup report is rewritten into.
    stderr: PathBuf,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `podman start`'s stderr, which is the action's stderr file.
    Podman,
    /// `crun`'s JSON log.
    Crun,
}

impl Said {
    /// Podman's: what `podman start` wrote to the action's `stderr` file.
    pub(crate) fn podman(stderr: &Path) -> Self {
        Self {
            source: stderr.to_owned(),
            stderr: stderr.to_owned(),
            kind: Kind::Podman,
        }
    }

    /// `crun`'s: the error records of its `--log` file `log`, in the same words as
    /// Podman relays them (`crun: <message>`); the action's own stderr stays `stderr`.
    pub(crate) fn crun(log: &Path, stderr: &Path) -> Self {
        Self {
            source: log.to_owned(),
            stderr: stderr.to_owned(),
            kind: Kind::Crun,
        }
    }

    /// The runtime's name in "the container did not run to an exit: <it> reports ...".
    fn reports(&self) -> &'static str {
        match self.kind {
            Kind::Podman => "podman",
            Kind::Crun => "crun",
        }
    }

    /// What the words are called after the semicolon.
    fn label(&self) -> &'static str {
        match self.kind {
            Kind::Podman => "podman start",
            Kind::Crun => "crun log",
        }
    }

    /// At most [`SAID_MAX`] bytes of the words.
    pub(crate) async fn read(&self) -> std::io::Result<String> {
        use tokio::io::AsyncReadExt as _;
        let mut bytes = Vec::new();
        tokio::fs::File::open(&self.source)
            .await?
            .take(SAID_MAX)
            .read_to_end(&mut bytes)
            .await?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(match self.kind {
            Kind::Podman => text.trim().to_owned(),
            Kind::Crun => crun_words(&text),
        })
    }
}

/// The error records of a `crun --log-format=json` log, as `crun: <message>` joined by
/// `; ` (what Podman relays of them). A line that is not a JSON record, and a record of
/// another level, are left out.
pub(crate) fn crun_words(log: &str) -> String {
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["level"] == "error" || record["level"] == "fatal")
        .filter_map(|record| record["msg"].as_str().map(|msg| format!("crun: {msg}")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether the report is crun's exec of the program failing with `ETXTBSY`
/// (`exec container process `<program>`: Text file busy`): a file still open for
/// writing somewhere when it was executed. With the input cache that can be a fill's
/// descriptor a forked child held for a moment
/// (`docs/design/worker-cache.md#64-concurrency-and-fills`), so the driver starts the
/// container once more. A report that cannot be read is not one.
pub(crate) async fn text_file_busy(said: &Said) -> bool {
    said.read()
        .await
        .is_ok_and(|said| said.contains("Text file busy"))
}

/// A container that did not run to an exit (the runtime's `state` for it): the
/// action's exit 127 or 126 when crun reports it cannot find or execute the program,
/// with kbf's message and the runtime's words as its stderr; otherwise the farm's
/// failure, naming what the runtime said.
pub(crate) async fn not_run(
    spec: &ContainerSpec,
    said: &Said,
    state: &str,
) -> Result<i32, RuntimeError> {
    match answer(spec, said)
        .await
        .map_err(|e| failed(&said.source, &e))?
    {
        Ok(why) => Ok(why.exit_code()),
        Err(words) => Err(RuntimeError::Failed(format!(
            "the container did not run to an exit: {} reports {state:?}; {}: {words}",
            said.reports(),
            said.label()
        ))),
    }
}

/// Reads what the runtime said and, when crun reports its lookup of the program,
/// rewrites it as the action's stderr: why, or the runtime's words if not.
async fn answer(spec: &ContainerSpec, said: &Said) -> std::io::Result<Result<Unstartable, String>> {
    let words = said.read().await?;
    let program = spec.argv.first().map_or("", String::as_str);
    let Some(why) = classify(program, &words) else {
        return Ok(Err(words));
    };
    let message = message(why, program, &spec.env, &spec.working_directory);
    tracing::info!(container = %spec.name, "{message}");
    tokio::fs::write(
        &said.stderr,
        format!("{message}\n{}: {words}\n", said.label()),
    )
    .await?;
    Ok(Ok(why))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOT_FOUND: &str = "Error: unable to start container \"c\": crun: executable file \
        `env` not found in $PATH: No such file or directory: OCI runtime attempted to invoke \
        a command that was not found";

    /// Catches a lookup report for another program taken for this one's, Podman's
    /// wrapping taken for crun's report (a mount source it could not find, a cgroup
    /// file it may not write: the farm's), and the two kinds swapped.
    #[test]
    fn only_crun_reporting_the_lookup_counts() {
        assert_eq!(classify("env", NOT_FOUND), Some(Unstartable::NotFound));
        assert_eq!(classify("sh", NOT_FOUND), None, "another program");
        assert_eq!(classify("en", NOT_FOUND), None, "a prefix of the name");
        let denied = "Error: crun: open executable: Permission denied: OCI permission denied";
        assert_eq!(classify("x", denied), Some(Unstartable::NotExecutable));
        for farm in [
            "Error: crun: mount `/s/root` to `/kbf/root`: No such file or directory: OCI \
             runtime attempted to invoke a command that was not found",
            "Error: crun: write `pids.max`: Operation not permitted: OCI permission denied",
            "",
        ] {
            assert_eq!(classify("env", farm), None, "{farm}");
        }
        assert_eq!(Unstartable::NotFound.exit_code(), 127);
        assert_eq!(Unstartable::NotExecutable.exit_code(), 126);
    }

    /// Catches a Podman report the driver cannot read back taken for anything but the
    /// farm's failure, naming the file.
    #[tokio::test]
    async fn a_report_that_cannot_be_read_is_the_farms() {
        let stderr = Path::new("/nonexistent/kbf-lease/stderr");
        let outcome = not_run(
            &crate::podman::tests::spec(),
            &Said::podman(stderr),
            "created 0",
        )
        .await;
        assert_eq!(
            format!("{outcome:?}"),
            "Err(Failed(\"/nonexistent/kbf-lease/stderr: No such file or directory (os error 2)\"))"
        );
    }

    /// Catches the message not naming the program, or naming the wrong place it was
    /// looked up: the Command's `PATH` for a bare name, the working directory for a
    /// path.
    #[test]
    fn the_message_names_the_program_and_where_it_was_looked_up() {
        let path = [("PATH".to_owned(), "/a:b".to_owned())];
        assert_eq!(
            message(Unstartable::NotFound, "env", &[], ""),
            "kbf: the action's program `env` was not found on the Command's PATH: a name \
             without a slash is looked up there, and the Command sets no PATH (an action \
             gets only the Command's environment)"
        );
        assert_eq!(
            message(Unstartable::NotFound, "tool", &path, "pkg"),
            "kbf: the action's program `tool` was not found on the Command's PATH \"/a:b\""
        );
        assert_eq!(
            message(Unstartable::NotExecutable, "./data", &path, "pkg"),
            "kbf: the action's program `./data` is not an executable file in the container \
             (image and input root, working directory /kbf/root/pkg)"
        );
        assert!(message(Unstartable::NotFound, "/x", &[], "").ends_with("directory /kbf/root)"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join(format!("program-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    const LOG: &str = concat!(
        r#"{"level":"warning","msg":"cgroup v1 is deprecated","time":"2026-01-01T00:00:00Z"}"#,
        "\n",
        r#"{"level":"error","msg":"executable file `env` not found in $PATH: No such file or directory","time":"2026-01-01T00:00:01Z"}"#,
        "\n",
        "not json at all\n",
        r#"{"level":"fatal","msg":"open executable: Permission denied","time":"2026-01-01T00:00:02Z"}"#,
        "\n",
    );

    /// Catches a warning taken for a failure (a lease with a warning in crun's log is not
    /// a runtime failure), an error record lost, a line that is not a record breaking
    /// the rest, and the words not matching what the lookup classifier reads.
    #[test]
    fn crun_words_are_its_error_records_in_podmans_form() {
        let words = crun_words(LOG);
        assert_eq!(
            words,
            "crun: executable file `env` not found in $PATH: No such file or directory; \
             crun: open executable: Permission denied"
        );
        assert_eq!(crun_words(""), "");
        assert_eq!(crun_words("{\"level\":\"warning\",\"msg\":\"x\"}\n"), "");
        assert_eq!(classify("env", &words), Some(Unstartable::NotFound));
    }

    /// Catches the OCI path reading the action's stderr instead of crun's log, losing the
    /// rewrite of the lookup report into the action's stderr, and the message naming the
    /// wrong runtime.
    #[tokio::test]
    async fn a_crun_lookup_report_becomes_the_actions_exit_127() {
        let dir = scratch("crun-lookup");
        let log = dir.join("crun.log");
        let stderr = dir.join("stderr");
        std::fs::write(&log, LOG).expect("log");
        std::fs::write(&stderr, b"the action's own words\n").expect("stderr");
        let mut spec = crate::podman::tests::spec();
        spec.argv = vec!["env".to_owned()];
        let said = Said::crun(&log, &stderr);
        assert_eq!(not_run(&spec, &said, "exited 1").await.expect("127"), 127);
        let written = std::fs::read_to_string(&stderr).expect("stderr");
        assert!(
            written.starts_with("kbf: the action's program `env` was not found"),
            "{written}"
        );
        assert!(
            written.contains("\ncrun log: crun: executable file `env`"),
            "{written}"
        );

        // Not a lookup report: the farm's failure, naming crun.
        spec.argv = vec!["other".to_owned()];
        std::fs::write(
            &log,
            r#"{"level":"error","msg":"mount `/x` to `/kbf/root`: No such file or directory"}"#,
        )
        .expect("log");
        let farm = not_run(&spec, &said, "exited 1").await;
        let why = farm.expect_err("the farm's failure").to_string();
        assert!(
            why.contains("crun reports \"exited 1\"; crun log: crun: mount `/x`"),
            "{why}"
        );
        assert!(!text_file_busy(&said).await);
        std::fs::write(
            &log,
            r#"{"level":"error","msg":"exec container process `/kbf/root/t`: Text file busy"}"#,
        )
        .expect("log");
        assert!(text_file_busy(&said).await);
    }
}
