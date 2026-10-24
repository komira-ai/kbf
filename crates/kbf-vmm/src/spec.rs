//! What the parent process tells the VM helper (`kbf-vmm vm-run`) and what the helper
//! tells back: a [`VmSpec`] in a JSON file, [`Event`]s as JSON lines on stdout.
//!
//! A VM runs in its own process so that a probe can kill it with SIGKILL (P2), start it
//! as another user (G5) and count the running guests the way the host does.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A directory shared into the guest with its own virtiofs tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedShare {
    pub tag: String,
    pub path: PathBuf,
    /// When set, the share is a multiple-directory share that lists `path` under this name;
    /// otherwise a single-directory share.
    #[serde(default)]
    pub multiple_name: Option<String>,
    /// Whether the guest may write to it.
    #[serde(default)]
    pub read_only: bool,
}

/// One VM to run, from a bundle on disk (see `bundle`). There is no network device: the
/// type has no field for one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmSpec {
    /// The bundle directory (a clone, never the prepared base).
    pub bundle: PathBuf,
    pub cpus: u32,
    pub memory_mib: u64,
    /// Whether the VM has a graphics device (a display with no host window).
    pub graphics: bool,
    /// A directory the guest finds under its automount share, `/Volumes/My Shared Files`,
    /// in a folder named `kbf`.
    pub automount_dir: Option<PathBuf>,
    /// Directories shared under custom virtiofs tags (never the automount tag, which the
    /// system mounts itself) that the guest must mount itself.
    pub tagged: Vec<TaggedShare>,
    /// Vsock ports the host listens on; each guest connection is an [`Event::Vsock`].
    pub vsock_ports: Vec<u32>,
    /// Console port names; bytes the guest writes to one are an [`Event::Console`].
    pub console_ports: Vec<String>,
    /// When set, the frames a guest sends on the vsock ports are written into this
    /// directory (`sink`): the host's results directory, where the probes look for files.
    /// Without it a connection is only counted and closed.
    #[serde(default)]
    pub results_dir: Option<PathBuf>,
}

impl VmSpec {
    /// A spec with the sizes the probes use and nothing shared.
    #[must_use]
    pub fn minimal(bundle: PathBuf) -> Self {
        Self {
            bundle,
            cpus: 2,
            memory_mib: 4096,
            graphics: false,
            automount_dir: None,
            tagged: Vec::new(),
            vsock_ports: Vec::new(),
            console_ports: Vec::new(),
            results_dir: None,
        }
    }
}

/// One line the helper prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    /// The configuration validated; the start call is next.
    Configured { ms: u64 },
    /// The start call's completion handler reported success, `ms` after the helper began.
    Started { ms: u64 },
    /// The VM changed state (`running`, `stopped`, `paused`, `error`, ...).
    State { state: String, ms: u64 },
    /// A guest connected to a vsock port the host listens on.
    Vsock { port: u32 },
    /// The guest wrote `text` to a console port.
    Console { port: String, text: String },
    /// The VM stopped: the guest shut down after a shutdown request, or the parent closed
    /// the helper's stdin and the VM was force-stopped.
    Stopped { ms: u64 },
    /// A call failed. `code` is the Virtualization.framework error code name when there is
    /// one (`VirtualMachineLimitExceeded`).
    Error {
        stage: String,
        message: String,
        code: Option<String>,
    },
}

impl Event {
    /// The JSON line.
    #[must_use]
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parses one helper line.
    ///
    /// # Errors
    /// The line is not an event.
    pub fn parse(line: &str) -> Result<Self, String> {
        serde_json::from_str(line).map_err(|e| format!("not a helper event ({e}): {line}"))
    }
}

/// The line the parent writes to the helper's stdin to ask the guest to shut down (the
/// helper then waits for the guest to stop). Closing the stdin instead force-stops the VM.
pub const SHUTDOWN_COMMAND: &str = "shutdown";

/// The error code name the helper uses when a start is refused for the two-guest limit.
pub const LIMIT_EXCEEDED: &str = "VirtualMachineLimitExceeded";

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches an event that does not survive its own line (a renamed tag or field), for
    /// every variant the helper prints.
    #[test]
    fn every_event_round_trips() {
        let all = [
            Event::Configured { ms: 1 },
            Event::Started { ms: 2 },
            Event::State {
                state: "running".into(),
                ms: 3,
            },
            Event::Vsock { port: 5000 },
            Event::Console {
                port: "p".into(),
                text: "hi\n".into(),
            },
            Event::Stopped { ms: 4 },
            Event::Error {
                stage: "start".into(),
                message: "m".into(),
                code: Some(LIMIT_EXCEEDED.into()),
            },
        ];
        for e in all {
            let line = e.to_line();
            assert!(!line.contains('\n'), "{line}");
            assert_eq!(Event::parse(&line), Ok(e));
        }
    }

    /// Catches a parser that accepts a line that is not an event as one.
    #[test]
    fn a_line_that_is_not_an_event_is_refused() {
        assert!(Event::parse("hello").is_err());
        assert!(Event::parse(r#"{"event":"unknown"}"#).is_err());
    }

    /// Catches a spec file written before `results_dir` existed no longer loading, and a
    /// default that attaches a sink nobody asked for.
    #[test]
    fn a_spec_without_a_results_dir_still_loads_with_none() {
        let text = serde_json::to_string(&VmSpec::minimal("/b".into())).expect("json");
        let mut v: serde_json::Value = serde_json::from_str(&text).expect("value");
        v.as_object_mut().expect("object").remove("results_dir");
        let s: VmSpec = serde_json::from_value(v).expect("old spec");
        assert_eq!(s.results_dir, None);
    }

    /// Catches a spec that loses a field on its way to the helper's file.
    #[test]
    fn a_spec_round_trips_through_json() {
        let mut s = VmSpec::minimal("/b".into());
        s.graphics = true;
        s.automount_dir = Some("/share".into());
        s.tagged = vec![
            TaggedShare {
                tag: "t".into(),
                path: "/t".into(),
                multiple_name: None,
                read_only: false,
            },
            TaggedShare {
                tag: "u".into(),
                path: "/u".into(),
                multiple_name: Some("m".into()),
                read_only: true,
            },
        ];
        s.vsock_ports = vec![5000];
        s.console_ports = vec!["kbf.probe".into()];
        s.results_dir = Some("/share/results".into());
        let text = serde_json::to_string(&s).expect("json");
        assert_eq!(serde_json::from_str::<VmSpec>(&text).expect("parse"), s);
    }
}
