use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::rc::Rc;
use std::time::Duration;

use kbf_host::path::HostPath;
use kbf_host::step::Perms;
use kbf_host::testkit::{FakeExec, ScratchHost};

use super::*;
use crate::binary::Arch;
use crate::fixtures::*;
use crate::layout::{Layout, Platform};
use crate::run::{Ran, Runner};
use crate::state;

const ELF_X86: u8 = 0x3e;
const ELF_ARM: u8 = 0xb7;
const MACHO_ARM: u32 = 0x0100_000c;
const MACHO_X86: u32 = 0x0100_0007;

/// A host, an inbox and everything a stage reads, on a scratch root.
struct Rig {
    exec: ScratchHost,
    host: Layout,
    settings: Settings,
    trust: FakeExec,
    /// The inbox entry.
    entry: PathBuf,
    /// A place outside the entry (a file a stage must never read).
    outside: PathBuf,
}

impl Rig {
    fn new(name: &str, platform: Platform) -> Rig {
        let (exec, host) = scratch_layout(name, platform);
        // What a host has above the layout: a Linux one `/run` and `/var/lib`; a Mac
        // `/var/run` and `/var/db`, and, on both, whatever `/usr` has (not `/usr/local`
        // on a fresh Mac).
        let seeded: &[&str] = match platform {
            Platform::Linux => &["run", "var/lib", "usr/local", "etc"],
            Platform::MacOs => &["var/run", "var/db", "usr"],
        };
        for sub in seeded {
            fs::create_dir_all(exec.root.join(sub)).expect("seed");
        }
        let mut settings = Settings::defaults(&host);
        settings.run_as = me();
        settings.timeout = Duration::from_secs(30);
        settings.arch = match platform {
            Platform::Linux => Arch::X86_64,
            Platform::MacOs => Arch::Aarch64,
        };
        let outside = scratch_dir(&format!("stage-{name}-outside"));
        let entry = outside.join("entry");
        Rig {
            exec,
            host,
            settings,
            trust: trusted_host(),
            entry,
            outside,
        }
    }

    /// An inbox offering `COMMIT` with a `kbf` carrying `bytes`.
    fn kbf(&self, bytes: &[u8]) -> String {
        write_inbox(&self.entry, COMMIT, &[part("kbf", bytes)])
    }

    fn stage(&mut self, runner: &dyn Runner) -> Result<Staged, Refusal> {
        self.stage_with(runner, &Hooks::default(), Limits::DEFAULT)
    }

    fn stage_with(
        &mut self,
        runner: &dyn Runner,
        hooks: &Hooks,
        limits: Limits,
    ) -> Result<Staged, Refusal> {
        let mut ctx = Context {
            host: &self.host,
            settings: &self.settings,
            exec: &mut self.exec,
            trust: &self.trust,
            runner,
            owner: me_owner(),
            limits,
        };
        stage_with(&mut ctx, &self.entry, hooks)
    }

    fn at(&self, host_path: &str) -> PathBuf {
        self.exec.at(host_path)
    }

    /// The names under the scratch directory of stages: empty when each was removed.
    fn work_leftovers(&self) -> Vec<String> {
        let dir = self.exec.at(&self.host.work_dir().to_string());
        fs::read_dir(dir)
            .map(|d| {
                d.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn releases(&self) -> Vec<String> {
        fs::read_dir(self.at("/usr/local/kbf/releases"))
            .map(|d| {
                d.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn state(&self) -> Option<state::State> {
        state::read_unlocked(&self.host).expect("reads")
    }
}

fn code(result: Result<Staged, Refusal>) -> &'static str {
    result.expect_err("refused").code()
}

fn is_gh(spawn: &Spawn) -> bool {
    spawn.program.file_name().is_some_and(|n| n == "gh")
}

fn is_staged_binary(spawn: &Spawn) -> bool {
    spawn.args.iter().any(|a| a == "--version")
}

/// The happy path, and the whole contract of what a stage leaves behind: the release
/// installed root-owned (here the test user) under `releases/<commit>/` with the binary
/// 0755 and the bytes of the tarball's file; the state's `staged` set and nothing else in
/// it; no `current` or `previous` link (nothing is activated); the scratch directory gone;
/// the version facts read from the binary's own output; and the programs run in order:
/// `gh` for the tarball, then the staged binary (in the scratch `out/`, an empty
/// environment, an empty directory, as the unprivileged user).
#[test]
fn a_verified_release_is_staged_and_nothing_is_activated() {
    let mut rig = Rig::new("stage-ok", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    let runner = FakeRunner::ok(COMMIT);
    let staged = rig.stage(&runner).expect("staged");

    assert_eq!(staged.commit.as_str(), COMMIT);
    assert_eq!(staged.components, ["kbf"]);
    assert_eq!(staged.source_ref.as_str(), "refs/heads/main");
    assert_eq!(
        (staged.protocol, staged.min_protocol, staged.state_schema),
        (4, 3, 7)
    );
    assert_eq!(staged.version, "0.1.0");
    assert!(
        staged.log.contains(&format!("release {COMMIT}:")),
        "{}",
        staged.log
    );
    assert!(staged.log.contains(&format!("state: {COMMIT} staged")));

    let binary = rig.at(&format!("/usr/local/kbf/releases/{COMMIT}/kbf"));
    assert_eq!(
        fs::read(&binary).expect("installed"),
        elf(ELF_X86, "kbf build")
    );
    assert_eq!(
        fs::metadata(&binary).expect("stat").permissions().mode() & 0o7777,
        0o755
    );
    let state = rig.state().expect("a state");
    assert_eq!(state.staged.as_ref().map(CommitId::as_str), Some(COMMIT));
    assert_eq!(
        (state.current, state.previous, state.pending_confirm),
        (None, None, None)
    );
    assert!(fs::symlink_metadata(rig.at("/usr/local/kbf/current")).is_err());
    assert!(fs::symlink_metadata(rig.at("/usr/local/kbf/previous")).is_err());
    assert_eq!(rig.work_leftovers(), Vec::<String>::new());

    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert!(is_gh(&calls[0]));
    let run = &calls[1];
    assert!(run.program.ends_with("out/kbf"), "{:?}", run.program);
    assert_eq!(run.args, ["--version", "--json"]);
    assert!(run.env.is_empty());
    assert!(run.cwd.ends_with("empty"), "{:?}", run.cwd);
    assert_eq!(
        (run.run_as, run.timeout),
        (Some(me()), rig.settings.timeout)
    );
}

/// Catches an unpack or a run before the gate: when `gh` is asked, the scratch `out/` has
/// nothing in it and nothing was run; when it says no, no other program was run, nothing
/// was installed and no state was written.
///
/// Mutant seen red: the unpack loop moved before the attestation loop (`out/` holds the
/// unpacked `kbf` when `gh` is asked, and the staged binary is then run, though the
/// release is not attested).
#[test]
fn nothing_is_unpacked_or_run_before_the_attestation_holds() {
    let mut rig = Rig::new("stage-gate", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    let unpacked_when_asked = Rc::new(Cell::new(usize::MAX));
    let copies_when_asked = Rc::new(Cell::new(0));
    let (unpacked, copies) = (unpacked_when_asked.clone(), copies_when_asked.clone());
    let runner = FakeRunner::new(move |s| {
        let work = s.cwd.parent().expect("the stage's directory").to_owned();
        unpacked.set(fs::read_dir(work.join("out")).expect("out").count());
        copies.set(fs::read_dir(work.join("in")).expect("in").count());
        Ok(ran(1, "", "no attestation found"))
    });
    let err = rig.stage(&runner).expect_err("not attested");
    assert_eq!(err.code(), "not-attested");
    assert_eq!(
        unpacked_when_asked.get(),
        0,
        "out/ was empty when gh was asked"
    );
    assert_eq!(
        copies_when_asked.get(),
        2,
        "the tarball and the bundle were copied"
    );
    assert_eq!(runner.calls().len(), 1);
    assert!(rig.releases().is_empty());
    assert_eq!(rig.state(), None);
    assert_eq!(rig.work_leftovers(), Vec::<String>::new());
}

/// Catches a byte of the copy changing between the copy and the check being missed (the
/// copy is hashed, after it is made), and a change to the original after the copy
/// mattering (the original is not read again): flipping a byte of the copy is refused
/// with the sha mismatch before any program runs; flipping a byte of the original in the
/// inbox after the copy changes nothing, and the release is staged from the copy.
///
/// Mutant seen red: the hash taken of the original inbox file instead of the copy (the
/// flipped copy is then accepted by the stage, and only `gh` would notice).
#[test]
fn the_copy_is_what_is_hashed_and_the_original_is_not_read_again() {
    let flip = |path: &Path| {
        let mut bytes = fs::read(path).expect("read");
        bytes[10] ^= 1;
        fs::write(path, bytes).expect("write");
    };
    let mut rig = Rig::new("stage-flip-copy", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    let runner = FakeRunner::ok(COMMIT);
    let on_copy = |dir: &Path| flip(&dir.join("kbf.tar.gz"));
    let hooks = Hooks {
        after_copy: Some(&on_copy),
        ..Hooks::default()
    };
    let err = rig
        .stage_with(&runner, &hooks, Limits::DEFAULT)
        .expect_err("a byte flipped");
    assert_eq!(err.code(), "hash-mismatch");
    assert_eq!(
        err.to_string(),
        "\"kbf.tar.gz\" does not hash to the sha256 the offer names for it"
    );
    assert!(runner.calls().is_empty());

    let mut rig = Rig::new("stage-flip-original", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    let entry = rig.entry.clone();
    let on_original = move |_: &Path| flip(&entry.join("kbf.tar.gz"));
    let hooks = Hooks {
        after_copy: Some(&on_original),
        ..Hooks::default()
    };
    rig.stage_with(&FakeRunner::ok(COMMIT), &hooks, Limits::DEFAULT)
        .expect("the copy was good");
}

/// Catches an offered file that does not have the sha256 the offer names: the tarball, and
/// the bundle, each refused naming the file.
#[test]
fn a_file_that_is_not_the_one_offered_is_refused() {
    for (target, name) in [
        ("kbf.tar.gz", "kbf.tar.gz"),
        ("provenance.jsonl", "provenance.jsonl"),
    ] {
        let mut rig = Rig::new(&format!("stage-sha-{name}"), Platform::Linux);
        rig.kbf(&elf(ELF_X86, "kbf build"));
        let mut bytes = fs::read(rig.entry.join(target)).expect("read");
        bytes.push(b'x');
        fs::write(rig.entry.join(target), bytes).expect("write");
        let err = rig.stage(&FakeRunner::ok(COMMIT)).expect_err("changed");
        assert_eq!(err.code(), "hash-mismatch");
        assert!(err.to_string().starts_with(&format!("{name:?}")), "{err}");
    }
}

/// Catches an inbox file the stage reads that it should not: a symbolic link to a file
/// outside the entry (the secret must not be copied: the refusal is the symlink's, not a
/// hash mismatch), a FIFO (which must not hang the stage), a directory, a file with
/// another hard link, an empty one, one past its size cap, a file the offer does not name,
/// one it names that is not there, and `offer.json` itself being a link. Each is refused
/// with its own code, before any program runs.
///
/// Mutants seen red: `O_NOFOLLOW` removed from the opens (the symlink cases then fail
/// with a hash mismatch, because the secret was read through the link); the link count
/// check dropped; the unexpected-name check dropped.
#[test]
fn an_inbox_entry_with_a_link_fifo_hard_link_or_stranger_is_refused() {
    type Attack = fn(&Rig);
    let attacks: [(&str, &str, Attack); 9] = [
        ("symlink", "inbox-symlink", |rig| {
            fs::write(rig.outside.join("secret"), b"not yours").expect("secret");
            fs::remove_file(rig.entry.join("kbf.tar.gz")).expect("remove");
            symlink(rig.outside.join("secret"), rig.entry.join("kbf.tar.gz")).expect("link");
        }),
        ("bundle-symlink", "inbox-symlink", |rig| {
            fs::remove_file(rig.entry.join("provenance.jsonl")).expect("remove");
            symlink("kbf.tar.gz", rig.entry.join("provenance.jsonl")).expect("link");
        }),
        ("offer-symlink", "inbox-symlink", |rig| {
            fs::rename(rig.entry.join("offer.json"), rig.outside.join("offer.json")).expect("move");
            symlink(rig.outside.join("offer.json"), rig.entry.join("offer.json")).expect("link");
        }),
        ("fifo", "inbox-not-regular", |rig| {
            fs::remove_file(rig.entry.join("kbf.tar.gz")).expect("remove");
            // `mkfifo(1)`: rustix has no `mknodat` on macOS.
            let made = std::process::Command::new("mkfifo")
                .arg(rig.entry.join("kbf.tar.gz"))
                .status()
                .expect("run mkfifo");
            assert!(made.success(), "a fifo");
        }),
        ("directory", "inbox-not-regular", |rig| {
            fs::remove_file(rig.entry.join("kbf.tar.gz")).expect("remove");
            fs::create_dir(rig.entry.join("kbf.tar.gz")).expect("dir");
        }),
        ("hard-link", "inbox-hard-linked", |rig| {
            fs::hard_link(
                rig.entry.join("kbf.tar.gz"),
                rig.outside.join("second-name"),
            )
            .expect("link");
        }),
        ("empty", "inbox-empty", |rig| {
            fs::write(rig.entry.join("provenance.jsonl"), b"").expect("empty");
        }),
        ("stranger", "inbox-unexpected", |rig| {
            fs::write(rig.entry.join("extra.txt"), b"a stranger\n").expect("stranger");
        }),
        ("missing", "inbox-missing", |rig| {
            fs::remove_file(rig.entry.join("kbf.tar.gz")).expect("remove");
        }),
    ];
    for (name, want, attack) in attacks {
        let mut rig = Rig::new(&format!("stage-inbox-{name}"), Platform::Linux);
        rig.kbf(&elf(ELF_X86, "kbf build"));
        attack(&rig);
        let runner = FakeRunner::ok(COMMIT);
        assert_eq!(code(rig.stage(&runner)), want, "{name}");
        assert!(runner.calls().is_empty(), "{name}");
        assert!(rig.releases().is_empty(), "{name}");
        assert_eq!(rig.work_leftovers(), Vec::<String>::new(), "{name}");
    }
}

/// Catches a file swapped between the listing of the entry and its being opened (the
/// race a check by path and then an open would lose): the hook replaces the tarball with a
/// link to a secret after the offer was read, and the open, which does not follow links,
/// refuses it. And a hook that does nothing changes nothing.
#[test]
fn a_file_swapped_for_a_link_before_it_is_opened_is_refused() {
    let mut rig = Rig::new("stage-swap", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    fs::write(rig.outside.join("secret"), b"not yours").expect("secret");
    let secret = rig.outside.join("secret");
    let swapped = Rc::new(RefCell::new(Vec::new()));
    let log = swapped.clone();
    let swap = move |entry: &Path, name: &str| {
        log.borrow_mut().push(name.to_owned());
        if name == "kbf.tar.gz" {
            fs::remove_file(entry.join(name)).expect("remove");
            symlink(&secret, entry.join(name)).expect("link");
        }
    };
    let hooks = Hooks {
        before_open: Some(&swap),
        ..Hooks::default()
    };
    let runner = FakeRunner::ok(COMMIT);
    let err = rig
        .stage_with(&runner, &hooks, Limits::DEFAULT)
        .expect_err("swapped");
    assert_eq!(err.code(), "inbox-symlink");
    assert_eq!(*swapped.borrow(), ["kbf.tar.gz"]);
    assert!(runner.calls().is_empty());
}

/// Catches the size caps not applying: with small limits, an oversize offer, tarball and
/// bundle are each refused as oversize before they are copied.
#[test]
fn a_file_over_its_cap_is_refused() {
    let limits = |f: fn(&mut Limits)| {
        let mut l = Limits::DEFAULT;
        f(&mut l);
        l
    };
    let cases: [(&str, Limits, &str); 3] = [
        ("offer", limits(|l| l.offer = 10), "offer.json"),
        ("tarball", limits(|l| l.tarball = 10), "kbf.tar.gz"),
        ("bundle", limits(|l| l.bundle = 10), "provenance.jsonl"),
    ];
    for (name, l, file) in cases {
        let mut rig = Rig::new(&format!("stage-cap-{name}"), Platform::Linux);
        rig.kbf(&elf(ELF_X86, "kbf build"));
        let err = rig
            .stage_with(&FakeRunner::ok(COMMIT), &Hooks::default(), l)
            .expect_err("oversize");
        assert_eq!(err.code(), "inbox-oversize", "{name}");
        assert!(err.to_string().contains(file), "{err}");
    }
}

/// Catches an offer that is not an offer, and one that names a file the entry does not
/// hold, reaching the checks after it.
#[test]
fn an_offer_that_is_not_an_offer_is_refused() {
    let mut rig = Rig::new("stage-offer", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    fs::write(rig.entry.join("offer.json"), "{\"schema\":2}").expect("offer");
    assert_eq!(code(rig.stage(&FakeRunner::ok(COMMIT))), "offer-malformed");
    fs::write(rig.entry.join("offer.json"), "nonsense").expect("offer");
    assert_eq!(code(rig.stage(&FakeRunner::ok(COMMIT))), "offer-malformed");
}

/// Catches a tarball the attestation accepted but that is not one regular file named for
/// the component: two files, a `../` entry, a symlink entry, another name, a directory
/// prefix, and a file that is no archive. Each is refused, nothing is installed, and the
/// staged binary is not run.
///
/// Mutants seen red: the entry count check dropped (the tarball with two files is
/// installed); the name check dropped (the `../kbf` entry is unpacked).
#[test]
fn a_tarball_that_is_not_one_file_with_the_component_name_is_refused() {
    let binary = elf(ELF_X86, "kbf build");
    let link = Item {
        name: b"kbf",
        kind: b'2',
        data: b"",
    };
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "two",
            tar_gz(&[file("kbf", &binary), file("README", b"hi")]),
            "archive-entries",
        ),
        (
            "dotdot",
            tar_gz(&[Item {
                name: b"../kbf",
                kind: b'0',
                data: &binary,
            }]),
            "archive-name",
        ),
        ("symlink", tar_gz(&[link]), "archive-not-regular"),
        (
            "wrong-name",
            tar_gz(&[file("kbf-daemon", &binary)]),
            "archive-name",
        ),
        (
            "prefixed",
            tar_gz(&[file("bin/kbf", &binary)]),
            "archive-name",
        ),
        (
            "not-a-tarball",
            b"plain bytes".to_vec(),
            "archive-malformed",
        ),
    ];
    for (name, tarball, want) in cases {
        let mut rig = Rig::new(&format!("stage-archive-{name}"), Platform::Linux);
        write_inbox(
            &rig.entry,
            COMMIT,
            &[Part {
                name: "kbf",
                tarball,
            }],
        );
        let runner = FakeRunner::ok(COMMIT);
        assert_eq!(code(rig.stage(&runner)), want, "{name}");
        assert_eq!(runner.calls().len(), 1, "{name}: only gh ran");
        assert!(rig.releases().is_empty(), "{name}");
    }
}

/// Catches a staged binary that is not this host's kind being run or installed: an
/// attested file for the wrong platform (a Mach-O on Linux, an ELF on a Mac), the wrong
/// CPU on either, and a script, are each refused by the header, before the binary is run.
///
/// Mutants seen red: the kind comparison dropped, for each of the four cases in turn (the
/// wrong file is then run and installed).
#[test]
fn an_attested_binary_of_another_platform_is_refused_unrun() {
    let cases: [(&str, Platform, Vec<u8>); 6] = [
        ("macho-on-linux", Platform::Linux, macho(MACHO_ARM, "x")),
        ("wrong-cpu-on-linux", Platform::Linux, elf(ELF_ARM, "x")),
        (
            "script-on-linux",
            Platform::Linux,
            b"#!/usr/bin/env kbf-no-such-program\necho hi\n".to_vec(),
        ),
        ("elf-on-mac", Platform::MacOs, elf(ELF_ARM, "x")),
        ("wrong-cpu-on-mac", Platform::MacOs, macho(MACHO_X86, "x")),
        (
            "script-on-mac",
            Platform::MacOs,
            b"#!/usr/bin/env kbf-no-such-program\necho hi\n".to_vec(),
        ),
    ];
    for (name, platform, bytes) in cases {
        let mut rig = Rig::new(&format!("stage-platform-{name}"), platform);
        write_inbox(&rig.entry, COMMIT, &[part("kbf", &bytes)]);
        let runner = FakeRunner::ok(COMMIT);
        let err = rig.stage(&runner).expect_err(name);
        assert_eq!(err.code(), "wrong-platform", "{name}");
        assert!(
            runner.calls().iter().all(|c| !is_staged_binary(c)),
            "{name}: the binary was run"
        );
        assert!(rig.releases().is_empty(), "{name}");
    }
    let mut rig = Rig::new("stage-platform-message", Platform::Linux);
    write_inbox(&rig.entry, COMMIT, &[part("kbf", &macho(MACHO_ARM, "x"))]);
    let err = rig.stage(&FakeRunner::ok(COMMIT)).expect_err("macho");
    assert_eq!(
        err.to_string(),
        "kbf is a Mach-O arm64 binary, and this host runs an ELF x86_64 binary"
    );
}

/// Catches the version check being skipped or loosened: a binary that says another commit
/// (or that is a debug build, exits non-zero, prints no JSON, or hangs) is refused, and
/// not installed. Each has its own code.
///
/// Mutants seen red: the commit comparison dropped; the profile comparison dropped.
#[test]
fn a_binary_that_does_not_say_it_is_the_commits_release_build_is_refused() {
    type Answer = fn(&Spawn) -> io::Result<Ran>;
    let cases: [(&str, &str, Answer); 5] = [
        ("commit", "version-commit", |s| {
            Ok(play(s, OTHER, "release"))
        }),
        ("profile", "version-profile", |s| {
            Ok(play(s, COMMIT, "debug"))
        }),
        ("exit", "version-failed", |s| {
            Ok(if is_staged_binary(s) {
                ran(2, "", "usage")
            } else {
                play(s, COMMIT, "release")
            })
        }),
        ("garbage", "version-unreadable", |s| {
            Ok(if is_staged_binary(s) {
                ran(0, "kbf 0.1.0\n", "")
            } else {
                play(s, COMMIT, "release")
            })
        }),
        ("killed", "version-failed", |s| {
            Ok(if is_staged_binary(s) {
                Ran {
                    code: None,
                    ..ran(0, "", "")
                }
            } else {
                play(s, COMMIT, "release")
            })
        }),
    ];
    for (name, want, answer) in cases {
        let mut rig = Rig::new(&format!("stage-version-{name}"), Platform::Linux);
        rig.kbf(&elf(ELF_X86, "kbf build"));
        assert_eq!(code(rig.stage(&FakeRunner::new(answer))), want, "{name}");
        assert!(rig.releases().is_empty(), "{name}");
        assert_eq!(rig.state(), None, "{name}");
    }
}

/// Catches a staged binary that cannot be started, or that runs out of time, being read as
/// anything but a refusal.
#[test]
fn a_binary_that_cannot_run_or_hangs_is_refused() {
    let mut rig = Rig::new("stage-version-spawn", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    let cannot = FakeRunner::new(|s| {
        if is_staged_binary(s) {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        } else {
            Ok(play(s, COMMIT, "release"))
        }
    });
    assert_eq!(code(rig.stage(&cannot)), "tool-failed");
    let hangs = FakeRunner::new(|s| {
        Ok(Ran {
            timed_out: is_staged_binary(s),
            ..play(s, COMMIT, "release")
        })
    });
    assert_eq!(code(rig.stage(&hangs)), "timed-out");
}

/// Catches a release being replaced: staging the same commit again with the same bytes is
/// a no-op that succeeds, and with different bytes (another tarball for the same commit,
/// which the attestation, being for one tarball, would not have covered) is refused and the
/// release on disk stays as it was.
///
/// Mutant seen red: the "differs" comparison in the install dropped (the second release
/// overwrites the first).
#[test]
fn a_commit_staged_again_with_other_bytes_is_refused_never_replaced() {
    let mut rig = Rig::new("stage-restage", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "first build"));
    rig.stage(&FakeRunner::ok(COMMIT)).expect("staged");
    rig.stage(&FakeRunner::ok(COMMIT))
        .expect("the same release again");

    fs::remove_dir_all(&rig.entry).expect("new inbox");
    rig.kbf(&elf(ELF_X86, "second build"));
    let err = rig.stage(&FakeRunner::ok(COMMIT)).expect_err("other bytes");
    assert_eq!(err.code(), "release-differs");
    assert!(
        err.to_string().contains(&format!("releases/{COMMIT}/kbf")),
        "{err}"
    );
    let binary = rig.at(&format!("/usr/local/kbf/releases/{COMMIT}/kbf"));
    assert_eq!(fs::read(binary).expect("kept"), elf(ELF_X86, "first build"));
}

/// Catches a trust list that is not root's alone being used: with a directory above `gh`
/// others can write, the stage refuses naming it before it reads the inbox, and runs
/// nothing; and a settings file that others can write is refused the same way (load is
/// the caller's, tested in settings).
#[test]
fn an_untrusted_gh_stops_the_stage_before_the_inbox_is_read() {
    let mut rig = Rig::new("stage-untrusted", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    rig.trust.entries.insert(
        HostPath::new("/usr/local/libexec").expect("path"),
        kbf_host::exec::Entry::Dir {
            perms: Perms::new(0o775),
            owner: Owner { uid: 0, gid: 0 },
        },
    );
    let runner = FakeRunner::ok(COMMIT);
    let err = rig.stage(&runner).expect_err("untrusted");
    assert_eq!(err.code(), "untrusted-writable");
    assert!(
        err.to_string()
            .starts_with("/usr/local/libexec is not trusted"),
        "{err}"
    );
    assert!(runner.calls().is_empty());
}

/// Catches a release of two components (Linux: `kbf` and `kbf-daemon`) not being checked
/// and installed whole: `gh` is asked for each tarball, each binary is run, both are
/// installed and reported, the first one's version facts are the ones reported.
#[test]
fn a_release_of_two_binaries_is_checked_and_installed_whole() {
    let mut rig = Rig::new("stage-two", Platform::Linux);
    write_inbox(
        &rig.entry,
        COMMIT,
        &[
            part("kbf", &elf(ELF_X86, "kbf build")),
            part("kbf-daemon", &elf(ELF_X86, "daemon build")),
        ],
    );
    let runner = FakeRunner::ok(COMMIT);
    let staged = rig.stage(&runner).expect("staged");
    assert_eq!(staged.components, ["kbf", "kbf-daemon"]);
    let calls = runner.calls();
    assert_eq!(calls.iter().filter(|c| is_gh(c)).count(), 2);
    assert_eq!(calls.iter().filter(|c| is_staged_binary(c)).count(), 2);
    let dir = format!("/usr/local/kbf/releases/{COMMIT}");
    assert_eq!(
        fs::read(rig.at(&format!("{dir}/kbf-daemon"))).expect("daemon"),
        elf(ELF_X86, "daemon build")
    );
    assert!(staged.log.contains("also kbf-daemon ("), "{}", staged.log);
}

/// Catches the macOS layout: a stage on a Mac uses `/var/db/kbf-deploy` for the state and
/// scratch directory and `/var/run/kbf-deploy.lock` for the lock, and never `/var/lib` or
/// `/run`; the missing parents are made (`/usr/local`, the state directory) 0755 and the
/// owner the stage was given; the release holds `kbf`.
///
/// Mutant seen red: the Linux paths used on a Mac (the lock's directory `/run` is not
/// there, and the stage fails before anything is staged).
#[test]
fn a_mac_stage_uses_the_mac_layout_and_makes_its_missing_parents() {
    let mut rig = Rig::new("stage-mac", Platform::MacOs);
    write_inbox(
        &rig.entry,
        COMMIT,
        &[part("kbf", &macho(MACHO_ARM, "daemon"))],
    );
    assert!(!rig.at("/usr/local").exists() && !rig.at("/var/db/kbf-deploy").exists());
    let runner = FakeRunner::ok(COMMIT);
    let staged = rig.stage(&runner).expect("staged");
    assert_eq!(staged.components, ["kbf"]);

    assert!(rig.at("/var/db/kbf-deploy/state.json").is_file());
    assert!(rig.at("/var/run/kbf-deploy.lock").is_file());
    assert!(!rig.at("/var/lib").exists());
    assert!(!rig.at("/run").exists());
    let me = me_owner();
    for dir in [
        "/usr/local",
        "/var/db/kbf-deploy",
        "/usr/local/kbf",
        "/usr/local/kbf/releases",
    ] {
        let meta = fs::metadata(rig.at(dir)).expect(dir);
        assert_eq!(meta.permissions().mode() & 0o7777, 0o755, "{dir}");
        assert_eq!(
            std::os::unix::fs::MetadataExt::uid(&meta),
            me.uid,
            "{dir} is the stage's owner's"
        );
    }
    let binary = rig.at(&format!("/usr/local/kbf/releases/{COMMIT}/kbf"));
    assert_eq!(
        fs::read(binary).expect("installed"),
        macho(MACHO_ARM, "daemon")
    );
    assert!(
        !rig.at(&format!("/usr/local/kbf/releases/{COMMIT}/kbf-daemon"))
            .exists()
    );
    assert_eq!(
        rig.state()
            .expect("state")
            .staged
            .as_ref()
            .map(CommitId::as_str),
        Some(COMMIT)
    );
}

/// Catches a Mac stage taking a release built for the old darwin layout: the Mac's
/// LaunchDaemon runs `current/kbf daemon`, and no `kbf-daemon` is built for darwin. An
/// offer with only `kbf-daemon`, and one with `kbf` and `kbf-daemon`, are refused before
/// any file is read or tool run (`offer-darwin-daemon`) and nothing is staged. A Linux host still takes `kbf-daemon`
/// alone, as it always did, and a Mac takes `kbf` alone (the test above).
///
/// Mutants seen red: the platform rule dropped (the Mac stages the `kbf-daemon` release
/// and no code is returned); the rule applied on Linux (the Linux stage is refused).
#[test]
fn a_mac_offer_may_not_name_kbf_daemon() {
    let macho = macho(MACHO_ARM, "daemon");
    let cases: [(&str, &[Part]); 2] = [
        ("daemon-only", &[part("kbf-daemon", &macho)]),
        ("both", &[part("kbf", &macho), part("kbf-daemon", &macho)]),
    ];
    for (name, parts) in cases {
        let mut rig = Rig::new(&format!("stage-mac-offer-{name}"), Platform::MacOs);
        write_inbox(&rig.entry, COMMIT, parts);
        let runner = FakeRunner::ok(COMMIT);
        let err = rig.stage(&runner).expect_err(name);
        assert_eq!(err.code(), "offer-darwin-daemon", "{name}: {err}");
        assert!(
            runner.calls().is_empty(),
            "{name}: no tool ran: {:?}",
            runner.calls()
        );
        assert!(rig.releases().is_empty(), "{name}: nothing staged");
    }

    let mut rig = Rig::new("stage-linux-daemon-only", Platform::Linux);
    write_inbox(
        &rig.entry,
        COMMIT,
        &[part("kbf-daemon", &elf(ELF_X86, "daemon build"))],
    );
    let staged = rig.stage(&FakeRunner::ok(COMMIT)).expect("linux staged");
    assert_eq!(staged.components, ["kbf-daemon"]);
}

/// Catches the macOS checks being skipped or run out of order: for each offered file
/// `xattr` is asked before anything is copied; `gh` before `codesign`; `codesign
/// --verify` and `--display` before the binary is run; and a release with a quarantined
/// file or a signature that is not CI's is refused, with the later programs not run.
///
/// Mutants seen red: the quarantine check dropped (a quarantined tarball is staged);
/// the signature check dropped (a binary with other flags is staged).
#[test]
fn the_mac_checks_run_in_order_and_each_refuses() {
    let program = |s: &Spawn| {
        s.program
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned()
    };
    let mut rig = Rig::new("stage-mac-order", Platform::MacOs);
    write_inbox(
        &rig.entry,
        COMMIT,
        &[part("kbf", &macho(MACHO_ARM, "daemon"))],
    );
    let runner = FakeRunner::ok(COMMIT);
    rig.stage(&runner).expect("staged");
    let order: Vec<String> = runner.calls().iter().map(program).collect();
    assert_eq!(
        order,
        ["xattr", "xattr", "gh", "codesign", "codesign", "kbf"]
    );
    let xattr = &runner.calls()[0];
    assert!(
        xattr
            .args
            .iter()
            .any(|a| a.to_str().is_some_and(|a| a.ends_with("kbf.tar.gz")))
    );

    let mut rig = Rig::new("stage-mac-quarantine", Platform::MacOs);
    write_inbox(
        &rig.entry,
        COMMIT,
        &[part("kbf", &macho(MACHO_ARM, "daemon"))],
    );
    let quarantined = FakeRunner::new(|s| {
        Ok(if s.program.ends_with("xattr") {
            ran(0, "0081;5f;Safari;\n", "")
        } else {
            play(s, COMMIT, "release")
        })
    });
    let err = rig.stage(&quarantined).expect_err("quarantined");
    assert_eq!(err.code(), "quarantined");
    assert_eq!(quarantined.calls().len(), 1, "stopped at the first file");
    assert!(rig.releases().is_empty());

    let mut rig = Rig::new("stage-mac-signature", Platform::MacOs);
    write_inbox(
        &rig.entry,
        COMMIT,
        &[part("kbf", &macho(MACHO_ARM, "daemon"))],
    );
    let unsigned = FakeRunner::new(|s| {
        let verify = s.program.ends_with("codesign") && s.args[0] == "--verify";
        Ok(if verify {
            ran(1, "", "not signed")
        } else {
            play(s, COMMIT, "release")
        })
    });
    let err = rig.stage(&unsigned).expect_err("unsigned");
    assert_eq!(err.code(), "signature-invalid");
    assert!(unsigned.calls().iter().all(|c| !is_staged_binary(c)));
    assert!(rig.releases().is_empty());
}

/// Catches the Mac checks running on Linux: nothing asks `xattr` or `codesign` there.
#[test]
fn the_mac_tools_are_not_run_on_linux() {
    let mut rig = Rig::new("stage-linux-no-mac-tools", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    let runner = FakeRunner::ok(COMMIT);
    rig.stage(&runner).expect("staged");
    assert!(
        runner
            .calls()
            .iter()
            .all(|c| !c.program.ends_with("xattr") && !c.program.ends_with("codesign"))
    );
}

/// Catches the scratch directory's handling: a directory a crashed stage left is not
/// touched and not reused; the stage's own is removed after a success and after a
/// refusal; and a scratch directory that cannot be made is a refusal of its own, not a
/// panic.
#[test]
fn the_scratch_directory_is_fresh_removed_and_its_failure_a_refusal() {
    let mut rig = Rig::new("stage-work", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    let work = rig.at(&rig.host.work_dir().to_string());
    let pid = std::process::id();
    for n in 0..2 {
        fs::create_dir_all(work.join(format!("stage.{pid}.{n}"))).expect("left behind");
    }
    rig.stage(&FakeRunner::ok(COMMIT)).expect("staged");
    let mut left = rig.work_leftovers();
    left.sort();
    assert_eq!(left, [format!("stage.{pid}.0"), format!("stage.{pid}.1")]);

    // The work directory cannot be written: the stage's directory cannot be made.
    fs::set_permissions(&work, fs::Permissions::from_mode(0o555)).expect("read-only");
    let err = rig.stage(&FakeRunner::ok(COMMIT)).expect_err("cannot make");
    assert_eq!(err.code(), "work-dir");
    fs::set_permissions(&work, fs::Permissions::from_mode(0o755)).expect("restore");

    // A file where the state directory should be: not even the parent can be made.
    let mut rig = Rig::new("stage-work-file", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    fs::write(rig.at("/var/lib/kbf-deploy"), b"a file").expect("seed");
    assert_eq!(code(rig.stage(&FakeRunner::ok(COMMIT))), "work-dir");
}

/// Catches a failing install being read as a staged release: a file where the release
/// directory's parent should be makes the copy fail, and the stage reports that as an
/// install failure, with nothing recorded as staged.
#[test]
fn a_failing_install_is_reported_and_records_nothing() {
    let mut rig = Rig::new("stage-install-fails", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    fs::write(rig.at("/usr/local/kbf"), b"a file").expect("seed");
    let err = rig
        .stage(&FakeRunner::ok(COMMIT))
        .expect_err("cannot install");
    assert_eq!(err.code(), "install-failed");
    assert_eq!(rig.state(), None);
    assert_eq!(rig.work_leftovers(), Vec::<String>::new());
}

/// Catches the public entry point differing from the hooked one: with no hooks it stages
/// the same release.
#[test]
fn the_entry_point_without_hooks_stages_the_release() {
    let mut rig = Rig::new("stage-entry", Platform::Linux);
    rig.kbf(&elf(ELF_X86, "kbf build"));
    let runner = FakeRunner::ok(COMMIT);
    let mut ctx = Context {
        host: &rig.host,
        settings: &rig.settings,
        exec: &mut rig.exec,
        trust: &rig.trust,
        runner: &runner,
        owner: me_owner(),
        limits: Limits::DEFAULT,
    };
    let staged = stage_from_inbox(&mut ctx, &rig.entry).expect("staged");
    assert_eq!(staged.commit.as_str(), COMMIT);
}
