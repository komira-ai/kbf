use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;

use super::install::{self, Done};
use super::manifest::EnvoyPin;
use super::testing::*;
use super::{EnvoyError, EnvoyId, VersionFault};
use crate::binary::BinaryKind;
use crate::fixtures::{FakeRunner, me, ran, sha256_hex};
use crate::layout::Platform;
use crate::state::{self, EnvoyPins, Locked};

/// What a fake `envoy --version` answers to a spawn.
type Answer = Box<dyn Fn(&crate::run::Spawn) -> std::io::Result<crate::run::Ran>>;

/// The installed build's directory on the test host.
fn build_dir(dir: &Path, id: &EnvoyId) -> std::path::PathBuf {
    envoy_root(dir).join(id.as_str())
}

fn pins(host: &crate::layout::Layout) -> EnvoyPins {
    state::read_unlocked(host)
        .expect("state")
        .map(|s| s.envoy)
        .unwrap_or_default()
}

fn work_left(dir: &Path) -> Vec<String> {
    names(&dir.join("var/lib/kbf-deploy/work"))
}

/// Fetch `bytes` on a fresh host with the fake Envoy runner; the host, its directory and
/// what was fetched.
fn fetched(name: &str, bytes: &[u8]) -> (crate::layout::Layout, std::path::PathBuf, EnvoyId) {
    let (host, dir) = scratch_host(name);
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.to_vec()));
    let done = install::fetch(&env(&host, &runner, &fetcher), &pin(bytes)).expect("fetch");
    assert_eq!(done.done, Done::Installed);
    (host, dir, id(bytes))
}

/// Catches a fetch that installs the wrong thing or leaves the wrong mark: it asks the
/// fetcher for the manifest's URL (into a directory of its own, with the size cap), the
/// build lands in `envoy/<version>-<sha[:16]>/` as `envoy` (0755) with `envoy.sha256`
/// (the full sha256 and a newline, 0644), `state.json` pins it as current with no
/// previous, `--version` ran as the verify user with no environment in an empty
/// directory, and no scratch directory is left.
///
/// Mutants seen red: the mode left at the umask's; `envoy.sha256` without the newline;
/// the state not written; the version run with the caller's environment.
#[test]
fn a_fetch_installs_the_build_and_pins_it() {
    let bytes = envoy();
    let (host, dir) = scratch_host("fetch");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
    let e = env(&host, &runner, &fetcher);
    let done = install::fetch(&e, &pin(&bytes)).expect("fetch");

    let id = id(&bytes);
    assert_eq!(done.id, id);
    assert_eq!(done.dir, build_dir(&dir, &id));
    assert_eq!(done.done, Done::Installed);
    assert_eq!(
        done.pins,
        EnvoyPins {
            current: Some(id.clone()),
            previous: None
        }
    );
    assert_eq!(pins(&host), done.pins);

    let binary = done.dir.join("envoy");
    assert_eq!(fs::read(&binary).expect("binary"), bytes);
    assert_eq!(fs::metadata(&binary).expect("stat").mode() & 0o7777, 0o755);
    let recorded = done.dir.join("envoy.sha256");
    assert_eq!(
        fs::read_to_string(&recorded).expect("sha"),
        format!("{}\n", sha256_hex(&bytes))
    );
    assert_eq!(
        fs::metadata(&recorded).expect("stat").mode() & 0o7777,
        0o644
    );
    assert_eq!(
        fs::metadata(&done.dir).expect("stat").mode() & 0o7777,
        0o755
    );
    assert_eq!(names(&envoy_root(&dir)), vec![id.to_string()]);

    let calls = fetcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, URL);
    assert_eq!(calls[0].2, 1024 * 1024);
    assert!(calls[0].1.ends_with("download/envoy"), "{:?}", calls[0].1);

    let spawns = runner.calls();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].args, vec![std::ffi::OsString::from("--version")]);
    assert!(spawns[0].env.is_empty());
    assert_eq!(spawns[0].run_as, Some(me()));
    assert!(spawns[0].cwd.ends_with("empty"), "{:?}", spawns[0].cwd);
    assert!(work_left(&dir).is_empty(), "{:?}", work_left(&dir));
}

/// Catches a download believed without its hash: another file's bytes under the
/// manifest's sha256 are refused with the two hashes, before anything is run, with
/// nothing installed, nothing pinned and no scratch left.
///
/// Mutant seen red: the hash comparison dropped (the wrong bytes install).
#[test]
fn a_download_with_the_wrong_sha256_is_refused_unrun() {
    let want = envoy();
    let served = fake_envoy(62, "9.9.9");
    let (host, dir) = scratch_host("hash");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(served.clone()));
    let err = install::fetch(&env(&host, &runner, &fetcher), &pin(&want)).expect_err("refused");
    match &err {
        EnvoyError::Hash { want: w, got } => {
            assert_eq!(*w, sha256_hex(&want));
            assert_eq!(*got, sha256_hex(&served));
        }
        other => panic!("{other}"),
    }
    assert_eq!((err.code(), err.exit()), ("envoy-hash", 3));
    assert!(
        runner.calls().is_empty(),
        "nothing is run before the hash holds"
    );
    assert!(names(&envoy_root(&dir)).is_empty());
    assert_eq!(pins(&host), EnvoyPins::default());
    assert!(work_left(&dir).is_empty());
}

/// Catches a binary for another CPU, or no binary at all, being installed: the right
/// hash on an aarch64 ELF, on a shell script and on a Mach-O is refused as the wrong
/// kind of binary, unrun.
///
/// Mutant seen red: the kind check dropped (the script would be run and installed).
#[test]
fn a_binary_for_another_cpu_or_no_binary_is_refused_unrun() {
    let script = b"#!/bin/sh\necho 'envoy  version: x/1.31.2/Clean/RELEASE/BoringSSL'\n".to_vec();
    let macho = crate::fixtures::macho(0x0100_000c, "x");
    for (name, bytes, found) in [
        (
            "arm",
            fake_envoy(183, VERSION),
            BinaryKind::Elf(crate::binary::Arch::Aarch64),
        ),
        ("script", script, BinaryKind::Other),
        (
            "macho",
            macho,
            BinaryKind::MachO(crate::binary::Arch::Aarch64),
        ),
    ] {
        let (host, dir) = scratch_host(name);
        let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
        let err = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect_err(name);
        match &err {
            EnvoyError::WrongBinary { found: f, want } => {
                assert_eq!(*f, found, "{name}");
                assert_eq!(*want, BinaryKind::Elf(crate::binary::Arch::X86_64));
            }
            other => panic!("{name}: {other}"),
        }
        assert_eq!((err.code(), err.exit()), ("envoy-wrong-platform", 3));
        assert!(runner.calls().is_empty(), "{name}: nothing is run");
        assert!(names(&envoy_root(&dir)).is_empty(), "{name}");
    }
}

/// Catches a build that does not say it is the manifest's version being installed:
/// another version, a failing `--version`, no version line and a program that ran out of
/// time each refuse (or fail) the fetch with nothing installed or pinned.
///
/// Mutants seen red: the version comparison dropped; a non-zero exit read as success.
#[test]
fn a_version_that_is_not_the_pins_is_refused() {
    let bytes = envoy();
    let cases: Vec<(&str, Answer)> = vec![
        ("other", Box::new(|_| Ok(ran(0, &says(OTHER_VERSION), "")))),
        ("exit", Box::new(|_| Ok(ran(1, &says(VERSION), "boom")))),
        ("garbage", Box::new(|_| Ok(ran(0, "no version here\n", "")))),
        (
            "short",
            Box::new(|_| Ok(ran(0, "envoy  version: abc\n", ""))),
        ),
        (
            "timeout",
            Box::new(|_| {
                let mut r = ran(0, "", "");
                r.code = None;
                r.timed_out = true;
                Ok(r)
            }),
        ),
        (
            "cannot-start",
            Box::new(|_| Err(std::io::Error::from_raw_os_error(libc::ENOEXEC))),
        ),
    ];
    for (name, answer) in cases {
        let (host, dir) = scratch_host(name);
        let (runner, fetcher) = (FakeRunner::new(answer), FakeFetcher::serving(bytes.clone()));
        let err = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect_err(name);
        let want = match name {
            "other" => (
                "envoy-version",
                3,
                matches!(&err, EnvoyError::Version(VersionFault::Mismatch { want, got })
                    if want == VERSION && got == OTHER_VERSION),
            ),
            "exit" => (
                "envoy-version",
                3,
                matches!(
                    &err,
                    EnvoyError::Version(VersionFault::Failed { code: Some(1) })
                ),
            ),
            "garbage" | "short" => (
                "envoy-version",
                3,
                matches!(&err, EnvoyError::Version(VersionFault::Unreadable)),
            ),
            "timeout" => ("timed-out", 1, matches!(&err, EnvoyError::TimedOut { .. })),
            _ => ("tool-failed", 1, matches!(&err, EnvoyError::Tool { .. })),
        };
        assert_eq!(
            (err.code(), err.exit(), want.2),
            (want.0, want.1, true),
            "{name}: {err}"
        );
        assert!(names(&envoy_root(&dir)).is_empty(), "{name}");
        assert_eq!(pins(&host), EnvoyPins::default(), "{name}");
        assert!(work_left(&dir).is_empty(), "{name}");
    }
}

/// Catches an existing build directory being overwritten: one holding other bytes (the
/// binary changed, the recorded sha256 changed, the sha256 file missing, the directory a
/// link) is refused before anything is downloaded, both by the plan and the fetch, and
/// every byte in it is as it was.
///
/// Mutants seen red: the install replacing the directory; the "same bytes" test looking
/// at the binary only.
#[test]
fn a_directory_holding_other_bytes_is_never_replaced() {
    let bytes = envoy();
    let id = id(&bytes);
    let pin = pin(&bytes);
    for (name, damage) in [
        ("binary", 0),
        ("recorded", 1),
        ("no-sha-file", 2),
        ("link", 3),
        ("empty", 4),
    ] {
        let (host, dir, _) = fetched(&format!("differs-{name}"), &bytes);
        let target = build_dir(&dir, &id);
        match damage {
            0 => fs::write(target.join("envoy"), b"someone else's\n").expect("write"),
            1 => fs::write(target.join("envoy.sha256"), format!("{}\n", "a".repeat(64)))
                .expect("write"),
            2 => fs::remove_file(target.join("envoy.sha256")).expect("rm"),
            3 => {
                let moved = envoy_root(&dir).join("moved");
                fs::rename(&target, &moved).expect("mv");
                symlink(&moved, &target).expect("link");
            }
            _ => {
                fs::remove_dir_all(&target).expect("rm");
                fs::create_dir(&target).expect("mkdir");
            }
        }
        let before: Vec<_> = names(&target);
        let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
        let e = env(&host, &runner, &fetcher);
        for result in [
            install::plan(&e, &pin).map(drop),
            install::fetch(&e, &pin).map(drop),
        ] {
            let err = result.expect_err(name);
            assert!(
                matches!(&err, EnvoyError::Differs { path } if *path == target),
                "{name}: {err}"
            );
            assert_eq!((err.code(), err.exit()), ("envoy-differs", 3));
        }
        assert!(fetcher.calls().is_empty(), "{name}: nothing is downloaded");
        assert_eq!(names(&target), before, "{name}");
        if damage == 0 {
            assert_eq!(
                fs::read(target.join("envoy")).expect("read"),
                b"someone else's\n"
            );
        }
    }
}

/// Catches a repeat that is not a no-op: fetching a build that is installed with the
/// same bytes downloads nothing, changes nothing in the directory (same inode, same
/// content) and leaves the state as it was; one with a stage directory left by a crashed
/// run still installs.
///
/// Mutants seen red: the repeat downloading again; the repeat rewriting the directory.
#[test]
fn a_repeat_of_the_same_build_changes_nothing() {
    let bytes = envoy();
    let (host, dir, id) = fetched("repeat", &bytes);
    let target = build_dir(&dir, &id);
    let inode = fs::metadata(target.join("envoy")).expect("stat").ino();
    let state_before = fs::read(dir.join("var/lib/kbf-deploy/state.json")).expect("state");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
    let again = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect("again");
    assert_eq!(again.done, Done::Unchanged);
    assert!(fetcher.calls().is_empty());
    assert_eq!(
        fs::metadata(target.join("envoy")).expect("stat").ino(),
        inode
    );
    assert_eq!(
        fs::read(dir.join("var/lib/kbf-deploy/state.json")).expect("state"),
        state_before
    );
    // The installed copy was checked again, not trusted for being there.
    assert_eq!(runner.calls().len(), 1);
    assert!(
        install::plan(&env(&host, &runner, &fetcher), &pin(&bytes))
            .expect("plan")
            .installed
    );

    // A stage a crashed run of this process left behind does not stop an install.
    let other = fake_envoy(62, "1.40.0");
    let other_pin = EnvoyPin {
        version: "1.40.0".to_owned(),
        ..self::pin(&other)
    };
    let other_id = EnvoyId::new("1.40.0", &sha256_hex(&other)).expect("id");
    let stale = envoy_root(&dir).join(format!(".stage-{other_id}.{}", std::process::id()));
    fs::create_dir_all(&stale).expect("stale");
    fs::write(stale.join("junk"), b"x").expect("junk");
    let fetcher = FakeFetcher::serving(other.clone());
    let done = install::fetch(&env(&host, &runner, &fetcher), &other_pin).expect("install");
    assert_eq!(done.done, Done::Installed);
    assert_eq!(names(&envoy_root(&dir)), {
        let mut both = vec![id.to_string(), other_id.to_string()];
        both.sort();
        both
    });
}

/// Catches the state's pin going wrong over a change of build: A then B pins B with A
/// as previous; A again swaps them back (and A is untouched, not downloaded); B again
/// changes nothing; an unrelated field of the state survives each write.
///
/// Mutants seen red: `previous` set to the new build; the old current dropped.
#[test]
fn the_state_keeps_the_current_and_the_previous_build() {
    let a = envoy();
    let b = fake_envoy(62, "1.40.0");
    let pin_b = EnvoyPin {
        version: "1.40.0".to_owned(),
        ..pin(&b)
    };
    let id_b = EnvoyId::new("1.40.0", &sha256_hex(&b)).expect("id");
    let (host, _dir, id_a) = fetched("rotate", &a);
    {
        let lock = Locked::acquire(&host).expect("lock");
        let mut state = lock.read().expect("read").expect("state");
        state.order_floor = 41;
        lock.write(&state).expect("write");
    }
    let (runner, fetcher_b) = (envoy_runner(), FakeFetcher::serving(b.clone()));
    let e = env(&host, &runner, &fetcher_b);
    let done = install::fetch(&e, &pin_b).expect("b");
    assert_eq!(
        done.pins,
        EnvoyPins {
            current: Some(id_b.clone()),
            previous: Some(id_a.clone())
        }
    );
    let fetcher_a = FakeFetcher::serving(a.clone());
    let back = install::fetch(&env(&host, &runner, &fetcher_a), &pin(&a)).expect("a again");
    assert_eq!(back.done, Done::Unchanged);
    assert!(fetcher_a.calls().is_empty());
    let want = EnvoyPins {
        current: Some(id_a.clone()),
        previous: Some(id_b.clone()),
    };
    assert_eq!(back.pins, want);
    assert_eq!(pins(&host), want);
    install::fetch(&env(&host, &runner, &fetcher_a), &pin(&a)).expect("a once more");
    assert_eq!(pins(&host), want);
    assert_eq!(
        state::read_unlocked(&host)
            .expect("state")
            .expect("some")
            .order_floor,
        41
    );
}

/// Catches a download that is not the fetch user's plain file being copied: a symbolic
/// link, a directory, a file over the cap, an empty file, nothing at all, and a file
/// another user owns are each refused (or, for a fetcher that wrote nothing, reported)
/// before the hash, with nothing run or installed.
///
/// Mutant seen red: the owner check dropped.
#[test]
fn a_download_that_is_not_the_fetch_users_plain_file_is_refused() {
    let bytes = envoy();
    let secret_free = |dest: &Path| {
        let target = dest.parent().expect("dir").join("elsewhere");
        fs::write(&target, envoy()).map_err(|e| failed(&e))?;
        symlink(&target, dest).map_err(|e| failed(&e))
    };
    let cases: Vec<(&str, FakeFetcher, &str)> = vec![
        ("link", FakeFetcher::new(secret_free), "inbox-symlink"),
        (
            "dir",
            FakeFetcher::new(|dest| fs::create_dir(dest).map_err(|e| failed(&e))),
            "inbox-not-regular",
        ),
        (
            "big",
            FakeFetcher::new(|dest| {
                fs::write(dest, vec![7u8; 2 * 1024 * 1024]).map_err(|e| failed(&e))
            }),
            "inbox-oversize",
        ),
        (
            "empty",
            FakeFetcher::new(|dest| fs::write(dest, b"").map_err(|e| failed(&e))),
            "inbox-empty",
        ),
        ("nothing", FakeFetcher::new(|_| Ok(())), "inbox-missing"),
    ];
    for (name, fetcher, code) in cases {
        let (host, dir) = scratch_host(name);
        let runner = envoy_runner();
        let err = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect_err(name);
        assert!(matches!(&err, EnvoyError::Received(_)), "{name}: {err}");
        assert_eq!(err.exit(), 3, "{name}");
        assert!(err.code() == code, "{name}: {}", err.code());
        assert!(runner.calls().is_empty(), "{name}");
        assert!(names(&envoy_root(&dir)).is_empty(), "{name}");
        assert!(work_left(&dir).is_empty(), "{name}");
    }
    // A file the fetch user does not own: the fetch user is someone else.
    let (host, dir) = scratch_host("owner");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
    let mut e = env(&host, &runner, &fetcher);
    e.fetch_as = crate::run::RunAs {
        uid: me().uid + 1,
        gid: me().gid,
    };
    let err = install::fetch(&e, &pin(&bytes)).expect_err("not theirs");
    assert!(
        matches!(err, EnvoyError::DownloadOwner { found, want } if found == me().uid && want == me().uid + 1),
        "{err}"
    );
    assert_eq!((err.code(), err.exit()), ("envoy-download-owner", 3));
    assert!(runner.calls().is_empty());
    assert!(names(&envoy_root(&dir)).is_empty());
}

/// Catches an install of different bytes than were checked: the copy is rewritten
/// between `--version` and the install (the runner plays a process that does it), and
/// the staged file's hash, taken as it is written, refuses it; nothing is installed.
///
/// Mutant seen red: the hash of the staged file not compared.
#[test]
fn the_staged_file_is_hashed_again_as_it_is_written() {
    let bytes = envoy();
    let evil = fake_envoy(62, VERSION);
    let evil = [evil.as_slice(), b"extra payload"].concat();
    let (host, dir) = scratch_host("swap");
    let runner = FakeRunner::new(move |spawn| {
        let said = ran(0, &says(VERSION), "");
        fs::write(&spawn.program, &evil)?;
        Ok(said)
    });
    let fetcher = FakeFetcher::serving(bytes.clone());
    let err = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect_err("refused");
    assert_eq!((err.code(), err.exit()), ("envoy-hash", 3), "{err}");
    assert!(
        names(&envoy_root(&dir)).iter().all(|n| n.starts_with('.'))
            || names(&envoy_root(&dir)).is_empty()
    );
    assert!(
        !names(&envoy_root(&dir))
            .iter()
            .any(|n| n.starts_with(".stage")),
        "the stage is removed"
    );
    assert_eq!(pins(&host), EnvoyPins::default());
}

/// Catches a fetch that fails quietly: a fetcher that cannot start, runs out of time or
/// fails is a failed fetch (exit 1) and installs nothing.
#[test]
fn a_fetcher_that_fails_fails_the_fetch() {
    let bytes = envoy();
    for fault in [
        super::fetch::FetchError::TimedOut,
        super::fetch::FetchError::Failed {
            status: "exit 22".to_owned(),
            detail: "404".to_owned(),
        },
    ] {
        let (host, dir) = scratch_host("fetch-fails");
        let runner = envoy_runner();
        let fetcher = FakeFetcher::new(move |_| {
            Err(match &fault {
                super::fetch::FetchError::TimedOut => super::fetch::FetchError::TimedOut,
                _ => super::fetch::FetchError::Failed {
                    status: "exit 22".to_owned(),
                    detail: "404".to_owned(),
                },
            })
        });
        let err = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect_err("fails");
        assert_eq!((err.code(), err.exit()), ("envoy-fetch", 1), "{err}");
        assert!(names(&envoy_root(&dir)).is_empty());
        assert!(work_left(&dir).is_empty());
    }
}

/// Catches Envoy being pinned on a host it is not for: on macOS every verb that acts
/// refuses with the platform, and exits 2.
#[test]
fn envoy_is_pinned_on_linux_only() {
    let dir = kbf_host::testkit::scratch("installer-envoy-macos");
    let host = crate::layout::Layout::new(
        kbf_host::path::Root::new(&dir).expect("absolute"),
        Platform::MacOs,
    );
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(envoy()));
    let e = env(&host, &runner, &fetcher);
    for err in [
        install::plan(&e, &pin(&envoy()))
            .map(drop)
            .expect_err("plan"),
        install::fetch(&e, &pin(&envoy()))
            .map(drop)
            .expect_err("fetch"),
        install::verify(&e).map(drop).expect_err("verify"),
        install::path(&e).map(drop).expect_err("path"),
    ] {
        assert!(
            matches!(err, EnvoyError::Platform(Platform::MacOs)),
            "{err}"
        );
        assert_eq!((err.code(), err.exit()), ("envoy-platform", 2));
    }
    assert!(fetcher.calls().is_empty());
}

/// Catches `verify` accepting what it should: after a fetch the pinned build passes and
/// the answer names the id, the full sha256 and the binary; `path` names the same file;
/// `--version` ran in the build's own directory as the verify user.
#[test]
fn verify_accepts_the_build_a_fetch_pinned() {
    let bytes = envoy();
    let (host, dir, id) = fetched("verify-ok", &bytes);
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(Vec::new()));
    let e = env(&host, &runner, &fetcher);
    let ok = install::verify(&e).expect("verify");
    assert_eq!(ok.id, id);
    assert_eq!(ok.sha256, sha256_hex(&bytes));
    assert_eq!(ok.binary, build_dir(&dir, &id).join("envoy"));
    assert_eq!(install::path(&e).expect("path"), ok.binary);
    let spawns = runner.calls();
    assert_eq!(spawns[0].program, ok.binary);
    assert_eq!(spawns[0].cwd, build_dir(&dir, &id));
    assert_eq!(spawns[0].run_as, Some(me()));
    assert!(fetcher.calls().is_empty(), "verify downloads nothing");
}

/// Catches each way an installed build stops being the pinned one and still starts:
/// a changed binary (the recorded hash no longer holds), a binary writable by group or
/// others, a directory or file owned by someone else, a binary replaced by a link, a
/// recorded sha256 that is malformed, missing or another build's, and a binary that now
/// reports another version. Each is refused, naming the thing, and exits 3.
///
/// Mutants seen red: the hash not re-checked; the mode check dropped; the owner check
/// dropped; the link accepted; the version not re-run.
#[test]
fn verify_refuses_a_build_that_changed_on_disk() {
    let bytes = envoy();
    type Damage = Box<dyn Fn(&std::path::Path)>;
    let cases: Vec<(&str, Damage, &str)> = vec![
        (
            "bytes",
            Box::new(|d| {
                fs::write(
                    d.join("envoy"),
                    fake_envoy(62, VERSION)
                        .iter()
                        .chain(b"!")
                        .copied()
                        .collect::<Vec<u8>>(),
                )
                .expect("w")
            }),
            "envoy-hash",
        ),
        (
            "mode",
            Box::new(|d| {
                fs::set_permissions(d.join("envoy"), fs::Permissions::from_mode(0o775))
                    .expect("chmod")
            }),
            "envoy-installed",
        ),
        (
            "dir-mode",
            Box::new(|d| fs::set_permissions(d, fs::Permissions::from_mode(0o757)).expect("chmod")),
            "envoy-installed",
        ),
        (
            "link",
            Box::new(|d| {
                let real = d.join("real");
                fs::rename(d.join("envoy"), &real).expect("mv");
                symlink(&real, d.join("envoy")).expect("link");
            }),
            "envoy-installed",
        ),
        (
            "missing-binary",
            Box::new(|d| fs::remove_file(d.join("envoy")).expect("rm")),
            "envoy-installed",
        ),
        (
            "sha-malformed",
            Box::new(|d| fs::write(d.join("envoy.sha256"), "nothex\n").expect("w")),
            "envoy-installed",
        ),
        (
            "sha-no-newline",
            Box::new(|d| fs::write(d.join("envoy.sha256"), sha256_hex(&envoy())).expect("w")),
            "envoy-installed",
        ),
        (
            "sha-other-build",
            Box::new(|d| {
                fs::write(d.join("envoy.sha256"), format!("{}\n", "b".repeat(64))).expect("w")
            }),
            "envoy-installed",
        ),
        (
            "sha-binary",
            Box::new(|d| {
                // Same prefix, other tail: the record and the build name agree, the bytes do not.
                let sha = sha256_hex(&envoy());
                fs::write(
                    d.join("envoy.sha256"),
                    format!("{}{}\n", &sha[..16], "0".repeat(48)),
                )
                .expect("w");
            }),
            "envoy-hash",
        ),
    ];
    for (name, damage, code) in cases {
        let (host, dir, id) = fetched(&format!("verify-{name}"), &bytes);
        damage(&build_dir(&dir, &id));
        let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(Vec::new()));
        let err = install::verify(&env(&host, &runner, &fetcher)).expect_err(name);
        assert_eq!((err.code(), err.exit()), (code, 3), "{name}: {err}");
    }
    // Owned by someone else.
    let (host, _dir, _id) = fetched("verify-owner", &bytes);
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(Vec::new()));
    let mut e = env(&host, &runner, &fetcher);
    e.owner_uid = me().uid + 1;
    let err = install::verify(&e).expect_err("owner");
    assert!(err.to_string().contains("owned by uid"), "{err}");
    assert!(runner.calls().is_empty(), "an unowned build is not run");
}

/// Catches a build that now says it is another version being accepted at the next start
/// (the binary and its record are unchanged; only what `--version` prints differs), and
/// an ELF for another CPU that a hand-made directory pins (its hash and name agree, so
/// only the machine check refuses it).
///
/// Mutants seen red: the version not compared in `verify`; the kind check missing there.
#[test]
fn verify_checks_the_version_and_the_machine_again() {
    let bytes = envoy();
    let (host, _dir, _id) = fetched("verify-version", &bytes);
    let runner = FakeRunner::new(|_| Ok(ran(0, &says(OTHER_VERSION), "")));
    let fetcher = FakeFetcher::serving(Vec::new());
    let err = install::verify(&env(&host, &runner, &fetcher)).expect_err("version");
    assert!(
        matches!(&err, EnvoyError::Version(VersionFault::Mismatch { got, .. }) if got == OTHER_VERSION),
        "{err}"
    );
    assert_eq!(err.exit(), 3);

    // An aarch64 build, installed and pinned by hand (a fetch would have refused it).
    let arm = fake_envoy(183, VERSION);
    let (host, dir) = scratch_host("verify-arm");
    let id = id(&arm);
    let target = build_dir(&dir, &id);
    fs::create_dir_all(&target).expect("dir");
    fs::write(target.join("envoy"), &arm).expect("binary");
    fs::set_permissions(target.join("envoy"), fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::write(
        target.join("envoy.sha256"),
        format!("{}\n", sha256_hex(&arm)),
    )
    .expect("sha");
    fs::set_permissions(
        target.join("envoy.sha256"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("chmod");
    for d in [&target, &envoy_root(&dir)] {
        fs::set_permissions(d, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    {
        let lock = Locked::acquire(&host).expect("lock");
        let mut state = lock.read().expect("read").unwrap_or_default();
        state.envoy.current = Some(id.clone());
        lock.write(&state).expect("write");
    }
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(Vec::new()));
    let err = install::verify(&env(&host, &runner, &fetcher)).expect_err("arm");
    assert!(matches!(err, EnvoyError::WrongBinary { .. }), "{err}");
    assert!(runner.calls().is_empty());
}

/// Catches `verify` and `path` passing with nothing pinned: no state file, a state with
/// no Envoy, and a pin whose binary is gone each fail; the first two need the operator
/// (exit 5), a state that is not a state needs the operator too, and a missing binary
/// is a refusal (exit 3).
#[test]
fn nothing_pinned_needs_the_operator() {
    let (host, dir) = scratch_host("unpinned");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(Vec::new()));
    let e = env(&host, &runner, &fetcher);
    for result in [install::verify(&e).map(drop), install::path(&e).map(drop)] {
        let err = result.expect_err("unpinned");
        assert!(matches!(err, EnvoyError::Unpinned));
        assert_eq!((err.code(), err.exit()), ("envoy-unpinned", 5));
    }
    {
        let lock = Locked::acquire(&host).expect("lock");
        lock.write(&crate::state::State::default()).expect("write");
    }
    assert!(matches!(install::verify(&e), Err(EnvoyError::Unpinned)));

    fs::write(dir.join("var/lib/kbf-deploy/state.json"), "{ not json").expect("corrupt");
    for result in [install::verify(&e).map(drop), install::list(&e).map(drop)] {
        let err = result.expect_err("corrupt");
        assert_eq!((err.code(), err.exit()), ("envoy-state", 5), "{err}");
    }

    let (host, dir, id) = fetched("path-gone", &envoy());
    fs::remove_file(build_dir(&dir, &id).join("envoy")).expect("rm");
    let err = install::path(&env(&host, &runner, &fetcher)).expect_err("gone");
    assert_eq!((err.code(), err.exit()), ("envoy-installed", 3));
}

/// Catches `list` reporting wrong: nothing (no `envoy/` yet) is an empty list; with two
/// builds the names sort, the pins are marked, and things in `envoy/` that are not builds
/// (a stage, a file, a bad name) are skipped.
///
/// Mutants seen red: current and previous swapped; a stage directory listed.
#[test]
fn list_names_the_builds_and_marks_the_pins() {
    let a = envoy();
    let b = fake_envoy(62, "1.40.0");
    let pin_b = EnvoyPin {
        version: "1.40.0".to_owned(),
        ..pin(&b)
    };
    let (host, dir) = scratch_host("list");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(a.clone()));
    assert!(
        install::list(&env(&host, &runner, &fetcher))
            .expect("empty")
            .is_empty()
    );
    install::fetch(&env(&host, &runner, &fetcher), &pin(&a)).expect("a");
    let fetcher_b = FakeFetcher::serving(b.clone());
    install::fetch(&env(&host, &runner, &fetcher_b), &pin_b).expect("b");
    let root = envoy_root(&dir);
    fs::create_dir(root.join(".stage-x.1")).expect("stage");
    fs::create_dir(root.join("scratch")).expect("bad");
    fs::write(root.join("1.1.1-0123456789abcdef"), b"a file").expect("file");
    let listed = install::list(&env(&host, &runner, &fetcher)).expect("list");
    let summary: Vec<_> = listed
        .iter()
        .map(|l| (l.id.to_string(), l.current, l.previous))
        .collect();
    let (id_a, id_b) = (
        id(&a).to_string(),
        EnvoyId::new("1.40.0", &sha256_hex(&b))
            .expect("id")
            .to_string(),
    );
    let mut want = vec![(id_a, false, true), (id_b, true, false)];
    want.sort();
    assert_eq!(summary, want);
}

/// Catches an unreadable or unwritable host being reported as something else: an
/// `envoy/` that is a file fails the plan and the listing as an I/O failure (exit 1, not a
/// refusal and not "nothing there"); a work directory a run left a name in is stepped
/// over; one that cannot be written to fails the fetch before anything is downloaded; a
/// build directory that cannot be entered is refused naming the error; handing the
/// download directory over fails loudly and is skipped for a caller that is not root.
///
/// Mutants seen red: a non-`NotFound` error read as "not installed"; the stale scratch
/// name reused.
#[test]
fn a_host_that_cannot_be_read_or_written_fails_the_verb() {
    let bytes = envoy();
    let (host, dir) = scratch_host("io-root-file");
    fs::create_dir_all(dir.join("var/lib/kbf-installer")).expect("tree");
    fs::write(envoy_root(&dir), b"a file").expect("file");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
    let e = env(&host, &runner, &fetcher);
    for err in [
        install::plan(&e, &pin(&bytes)).map(drop).expect_err("plan"),
        install::list(&e).map(drop).expect_err("list"),
    ] {
        assert_eq!((err.code(), err.exit()), ("work-dir", 1), "{err}");
    }

    // A name taken in the work directory is stepped over.
    let (host, dir) = scratch_host("io-stale-scratch");
    let work = dir.join("var/lib/kbf-deploy/work");
    let stale = work.join(format!("envoy.{}.0", std::process::id()));
    fs::create_dir_all(&stale).expect("stale");
    let fetcher = FakeFetcher::serving(bytes.clone());
    let done = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect("fetch");
    assert_eq!(done.done, Done::Installed);
    assert_eq!(
        names(&work),
        vec![
            stale
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        ]
    );

    // A work directory that cannot be written to.
    let (host, dir) = scratch_host("io-work-readonly");
    let work = dir.join("var/lib/kbf-deploy/work");
    fs::create_dir_all(&work).expect("work");
    fs::set_permissions(&work, fs::Permissions::from_mode(0o555)).expect("read-only");
    let fetcher = FakeFetcher::serving(bytes.clone());
    let err = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect_err("read-only");
    fs::set_permissions(&work, fs::Permissions::from_mode(0o755)).expect("restore");
    assert_eq!((err.code(), err.exit()), ("work-dir", 1), "{err}");
    assert!(fetcher.calls().is_empty());

    // A build directory that cannot be entered.
    let (host, dir, id) = fetched("io-no-search", &bytes);
    let root = envoy_root(&dir);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).expect("lock out");
    let result = install::verify(&env(&host, &runner, &fetcher));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("restore");
    let err = result.expect_err("no search");
    assert_eq!((err.code(), err.exit()), ("envoy-installed", 3), "{err}");
    assert!(err.to_string().contains(id.as_str()), "{err}");

    // Handing the download directory over.
    let nowhere = dir.join("no/such/dir");
    let err = install::hand_over(&nowhere, me(), true).expect_err("no dir");
    assert_eq!((err.code(), err.exit()), ("work-dir", 1));
    assert!(install::hand_over(&nowhere, me(), false).is_ok());
    assert!(install::hand_over(&dir, me(), true).is_ok());
    assert!(
        failed(&std::io::Error::other("why"))
            .to_string()
            .contains("why")
    );
}

/// Catches an install that clobbers a build another run installed meanwhile: while this
/// run's `--version` is going, the same build appears in `envoy/` (the runner plays the
/// other run); the install under the lock finds the same bytes there and leaves them,
/// and still pins the build.
///
/// Mutant seen red: `install_dir` not looking again under the lock (the rename then fails
/// on the directory that appeared).
#[test]
fn a_build_that_appears_meanwhile_is_left_alone() {
    let bytes = envoy();
    let id = id(&bytes);
    let (host, dir) = scratch_host("meanwhile");
    let target = build_dir(&dir, &id);
    let planted = bytes.clone();
    let there = target.clone();
    let runner = FakeRunner::new(move |spawn| {
        let said = ran(0, &says(VERSION), "");
        if !there.exists() {
            fs::create_dir_all(&there)?;
            fs::write(there.join("envoy"), &planted)?;
            fs::set_permissions(there.join("envoy"), fs::Permissions::from_mode(0o755))?;
            fs::write(
                there.join("envoy.sha256"),
                format!("{}\n", sha256_hex(&planted)),
            )?;
            fs::set_permissions(
                there.join("envoy.sha256"),
                fs::Permissions::from_mode(0o644),
            )?;
            fs::set_permissions(&there, fs::Permissions::from_mode(0o755))?;
            let _ = &spawn.program;
        }
        Ok(said)
    });
    let fetcher = FakeFetcher::serving(bytes.clone());
    let done = install::fetch(&env(&host, &runner, &fetcher), &pin(&bytes)).expect("fetch");
    assert_eq!(done.done, Done::Unchanged);
    assert_eq!(pins(&host).current, Some(id));
    assert_eq!(names(&envoy_root(&dir)).len(), 1);
}
