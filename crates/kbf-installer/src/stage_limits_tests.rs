//! The stage's bounds and its policy: a hanging `gh` or binary killed at its limit, an
//! extraction out of time, the partial directory's cleanup, and the policy read from
//! `installer.toml`. They share `stage_tests`' rig.

use std::cell::RefCell;
use std::io;

use super::*;

/// Runs the stand-in program for the program the stage asks to run, for real (so the
/// bounds are the real ones), and records what was asked.
struct Stub {
    gh: Option<(PathBuf, Vec<String>)>,
    probe: Option<(PathBuf, Vec<String>)>,
    seen: RefCell<Vec<Spawn>>,
}

impl Runner for Stub {
    fn run(&self, spawn: &Spawn) -> io::Result<Ran> {
        self.seen.borrow_mut().push(spawn.clone());
        let is_gh = spawn.program.file_name().is_some_and(|n| n == "gh");
        let stub = if is_gh { &self.gh } else { &self.probe };
        match stub {
            Some((program, args)) => SystemRunner.run(&Spawn {
                program: program.clone(),
                args: args.iter().map(Into::into).collect(),
                ..spawn.clone()
            }),
            None if is_gh => Ok(ran(0, "", "")),
            None => Ok(ran(0, &version_json(COMMIT, "release"), "")),
        }
    }
}

/// A long sleep that carries `marker` in its arguments, so that the test can tell it from
/// every other process: a stand-in for a program that never returns.
fn hanging_sleep(marker: &str) -> (PathBuf, Vec<String>) {
    (PathBuf::from("/bin/sleep"), vec![format!("300.{marker}")])
}

/// Whether no process is left whose arguments carry `marker` (the sleep was killed).
fn gone(marker: &str) -> bool {
    for _ in 0..200 {
        let found = std::process::Command::new("/usr/bin/pgrep")
            .args(["-f", &format!("300.{marker}")])
            .output()
            .expect("pgrep runs");
        if found.stdout.is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Catches an unbounded step: a `gh` that never returns, and a binary whose
/// `--version --json` never returns, are killed with their whole process group at their
/// own limit (here 300 ms in place of 120 s and 10 s), and the stage ends as `attestation`
/// and `facts`, in seconds, installing nothing. The stand-in (a long `sleep`) is dead too;
/// that a program's whole group is killed is `run`'s test.
///
/// Catches: the policy's limit not passed to `gh`; not passed to the probe.
#[test]
fn a_hanging_gh_and_a_hanging_binary_are_killed_at_their_limits() {
    for (who, word) in [("gh", "attestation"), ("probe", "facts")] {
        let mut rig = Rig::new(&format!("hang-{who}"));
        rig.policy.timeouts.gh = Duration::from_millis(300);
        rig.policy.timeouts.probe = Duration::from_millis(300);
        let marker = format!(
            "{}{}",
            std::process::id(),
            rig.dir.display().to_string().len()
        );
        let stand_in = hanging_sleep(&marker);
        let stub = Stub {
            gh: (who == "gh").then(|| stand_in.clone()),
            probe: (who == "probe").then_some(stand_in),
            seen: RefCell::default(),
        };
        let started = Instant::now();
        let err = rig.activate(&stub).expect_err(who);
        assert_eq!(err.word(), Some(word), "{who}: {err}");
        assert!(matches!(err, Refusal::TimedOut { .. }), "{who}: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{who} was bounded"
        );
        assert!(gone(&marker), "{who}: the stand-in is dead");
        rig.assert_nothing_installed();
    }
}

/// Catches the unpack taking the whole budget of the stage: an extraction given no time
/// ends as `shape`; the 120 s default is the one the pipeline passes.
#[test]
fn an_extraction_out_of_time_is_a_shape_refusal() {
    let mut rig = Rig::new("extract-time");
    rig.policy.timeouts.extract = Duration::ZERO;
    let runner = honest();
    let err = rig.activate(&runner).expect_err("no time");
    assert_eq!(err.word(), Some("shape"), "{err}");
    assert_eq!(err.code(), "archive-timeout");
    assert_eq!(programs(&runner), ["gh"]);
    rig.assert_nothing_installed();
    assert_eq!(Timeouts::DEFAULT.extract, Duration::from_secs(120));
}

/// Catches a partial directory that is not made as the contract says (0755, with `dl/`
/// 0700, owned as asked), a stale one being silently reused, and one left behind when the
/// holder is dropped.
#[test]
fn the_partial_directory_is_made_whole_and_removed_when_dropped() {
    let rig = Rig::new("partial");
    let partial = Partial::create(&rig.host, &commit(), owner()).expect("made");
    assert_eq!(mode_of_dir(partial.path()), 0o755);
    assert_eq!(mode_of_dir(&partial.dl()), 0o700);
    assert!(partial.path().ends_with(format!("{COMMIT}.partial")));
    let again = Partial::create(&rig.host, &commit(), owner()).expect_err("stale is refused");
    assert_eq!(again.code(), "work-dir");
    // The refused second holder removed nothing of the first's.
    assert!(partial.dl().is_dir());
    let path = partial.path().to_owned();
    drop(partial);
    assert!(!path.exists());
}

/// A file whose every number differs from the next's and from the defaults, so a value read
/// from the wrong field cannot pass as the right one.
fn tuned() -> InstallerToml {
    InstallerToml {
        max_component_bytes: 7_000_001,
        min_protocol: 4,
        min_generation: 9,
        fetch_timeout_s: 1234,
        min_activation_interval_s: 77,
        confirm_window_s: 301,
        ..InstallerToml::default()
    }
}

/// A host's policy is its `installer.toml`'s component cap and `min_protocol` floor with the
/// installer's own supported numbers, binary cap and timeouts: written out whole, so a number
/// taken from another field (`min_generation`, `fetch_timeout_s`), the floor lost, or the
/// supported numbers replaced by anything but [`supported`] fails.
///
/// Catches: `min_protocol` read from `min_generation`; `max_component_bytes` from
/// `fetch_timeout_s`; `supported` replaced by the test policy's 10/10/10.
#[test]
fn a_policy_is_the_installer_tomls_caps_and_floor_over_the_installers_own_numbers() {
    let want = Policy {
        max_component_bytes: 7_000_001,
        max_binary_bytes: MAX_BINARY,
        min_protocol: 4,
        supported: Supported {
            protocol: 1,
            min_protocol: 1,
            state_schema: u64::from(kbf_pack_index::schema::latest()),
        },
        timeouts: Timeouts::DEFAULT,
    };
    assert_eq!(Policy::of(&tuned()), want);
    // The defaults of an empty file: no floor, the 512 MiB cap.
    let plain = Policy::of(&InstallerToml::default());
    assert_eq!(
        (plain.min_protocol, plain.max_component_bytes),
        (0, 512 << 20)
    );
}

/// The numbers this installer understands are the first protocol and the schema it embeds:
/// a release that declares a protocol above 1 is `installer_too_old` until the installer is
/// changed on purpose (decision D3), and the schema is the database client's own newest, not
/// a copy. `kbf-cli`'s test pins them against what the server declares.
///
/// Catches: `protocol` 2; `state_schema` 0.
#[test]
fn the_supported_numbers_are_protocol_one_and_the_embedded_schema() {
    let s = supported();
    assert_eq!((s.protocol, s.min_protocol), (1, 1));
    assert_eq!(s.state_schema, u64::from(kbf_pack_index::schema::latest()));
    assert!(s.state_schema > 0, "this build embeds a migration");
}

#[path = "stage_version_tests.rs"]
mod version;
