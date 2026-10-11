use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;

use super::*;
use crate::linux::host::SYSTEMCTL;
use kbf_host::step::Step;
use kbf_host::testkit::{ScratchHost, reply};

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");
const FILE: &str = "/srv/swap/kbf.swap";
const OTHER_LINE: &str = "UUID=1 / ext4 defaults 0 1";
/// 20 GiB in KiB: room for a 1G file and the 16 GiB spare.
const ROOMY: u64 = 20 * 1024 * 1024;

fn config(file: &str) -> LinuxConfig {
    let text = EXAMPLE
        .replace("KBF_SWAP_FILE=/swapfile", &format!("KBF_SWAP_FILE={file}"))
        .replace("KBF_SWAP_SIZE=128G", "KBF_SWAP_SIZE=1G");
    LinuxConfig::parse(&text).expect("valid settings")
}

fn swap(file: &str) -> Swap {
    Swap {
        config: config(file),
        kubelet_allows_swap: false,
        remove: false,
    }
}

/// What the fake host's tools see.
#[derive(Default)]
struct World {
    active: BTreeSet<String>,
    swaps: Vec<String>,
    fs: String,
    free_kib: u64,
}

/// A scratch root playing the tools: `fallocate` makes a sparse file of that size under
/// the root, `chmod` sets its mode, `swapon`/`swapoff` change the active list.
fn host(name: &str) -> (ScratchHost, Rc<RefCell<World>>) {
    let mut exec = ScratchHost::new(name);
    let world = Rc::new(RefCell::new(World {
        fs: "ext4".to_owned(),
        free_kib: ROOMY,
        ..World::default()
    }));
    let (w, root) = (Rc::clone(&world), exec.root.clone());
    let under = move |p: &str| -> PathBuf { root.join(p.trim_start_matches('/')) };
    exec.answer = Box::new(move |cmd| {
        let mut w = w.borrow_mut();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        Ok(match (cmd.program.as_str(), &args[..]) {
            (SYSTEMCTL, ["is-active", "--quiet", unit]) => {
                reply(if w.active.contains(*unit) { 0 } else { 3 }, "", "")
            }
            (SWAPON, ["--show=NAME", "--noheadings"]) => reply(0, &w.swaps.join("\n"), ""),
            (SWAPON, [file]) => {
                w.swaps.push((*file).to_owned());
                reply(0, "", "")
            }
            (SWAPOFF, [file]) => {
                w.swaps.retain(|s| s != file);
                reply(0, "", "")
            }
            (FINDMNT, _) => reply(0, &format!("{}\n", w.fs), ""),
            (DF, _) => reply(
                0,
                &format!(
                    "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 999 1 {} 1% /\n",
                    w.free_kib
                ),
                "",
            ),
            (FALLOCATE, ["-l", "1G", file]) => {
                let f = fs::File::create(under(file)).expect("create");
                f.set_len(GIB).expect("sparse length");
                reply(0, "", "")
            }
            (CHMOD, ["600", file]) => {
                fs::set_permissions(under(file), fs::Permissions::from_mode(0o600)).expect("chmod");
                reply(0, "", "")
            }
            // mkswap.
            _ => reply(0, "", ""),
        })
    });
    fs::create_dir_all(exec.at("/srv/swap")).expect("swap dir");
    fs::create_dir_all(exec.at("/etc")).expect("etc");
    fs::write(exec.at("/etc/fstab"), OTHER_LINE).expect("fstab");
    fs::set_permissions(exec.at("/etc/fstab"), fs::Permissions::from_mode(0o644)).expect("chmod");
    (exec, world)
}

fn go(exec: &mut ScratchHost, a: &Swap, mode: Mode) -> String {
    let mut out = Vec::new();
    run(exec, a, mode, &mut out).expect("swap");
    String::from_utf8(out).expect("utf-8")
}

fn fstab(exec: &ScratchHost) -> String {
    fs::read_to_string(exec.at("/etc/fstab")).expect("fstab")
}

/// The plan's test on a scratch root: a dry run prints the five steps and changes
/// nothing; the apply makes a 1 GiB file, 0600, active and in fstab after the line
/// already there; a re-run plans nothing. Catches a step planned when the host already
/// has it, an fstab rewrite that loses or glues the existing line, and a swapfile left
/// readable by others.
#[test]
fn a_new_swapfile_is_made_activated_and_listed_and_a_rerun_plans_nothing() {
    let (mut exec, world) = host("swap-new");
    let a = swap(FILE);
    let dry = go(&mut exec, &a, Mode::DryRun);
    let line = format!("{OTHER_LINE}\n{FILE} none swap sw 0 0\n");
    let owner = fs::metadata(exec.at("/etc/fstab")).expect("stat");
    let want = format!(
        "DRY-RUN: run /usr/bin/fallocate -l 1G {FILE}\n\
         DRY-RUN: run /usr/bin/chmod 600 {FILE}\n\
         DRY-RUN: run /usr/sbin/mkswap {FILE}\n\
         DRY-RUN: run /usr/sbin/swapon {FILE}\n\
         DRY-RUN: write /etc/fstab (0644, owner {}:{}, {} bytes)\n\
         dry run: nothing changed. Re-run with --apply.\n",
        std::os::unix::fs::MetadataExt::uid(&owner),
        std::os::unix::fs::MetadataExt::gid(&owner),
        line.len()
    );
    assert_eq!(dry, want);
    assert!(!exec.at(FILE).exists() && exec.log.is_empty());

    go(&mut exec, &a, Mode::Apply);
    let meta = fs::metadata(exec.at(FILE)).expect("made");
    assert_eq!(
        (meta.len(), meta.permissions().mode() & 0o777),
        (GIB, 0o600)
    );
    assert_eq!(fstab(&exec), line);
    assert_eq!(world.borrow().swaps, [FILE]);

    let again = go(&mut exec, &a, Mode::DryRun);
    let want =
        format!("{FILE}: exists, 1G\n{FILE}: active\n/etc/fstab: has {FILE}\nnothing to do\n");
    assert_eq!(again, want);
}

/// Catches a rollback that leaves swap on, drops the other fstab lines with the
/// swapfile's, or keeps the file; and one that plans again once all is gone.
#[test]
fn remove_undoes_each_step_and_keeps_the_other_fstab_lines() {
    let (mut exec, world) = host("swap-remove");
    go(&mut exec, &swap(FILE), Mode::Apply);
    let a = Swap {
        remove: true,
        ..swap(FILE)
    };
    let dry = go(&mut exec, &a, Mode::DryRun);
    assert!(
        dry.starts_with(&format!(
            "DRY-RUN: run /usr/sbin/swapoff {FILE}\nDRY-RUN: write /etc/fstab"
        )),
        "{dry}"
    );
    assert!(dry.contains(&format!("DRY-RUN: remove {FILE}\n")), "{dry}");
    go(&mut exec, &a, Mode::Apply);
    assert_eq!(fstab(&exec), format!("{OTHER_LINE}\n"));
    assert!(!exec.at(FILE).exists());
    assert!(world.borrow().swaps.is_empty());
    let again = go(&mut exec, &a, Mode::DryRun);
    let want =
        format!("{FILE}: not active\n/etc/fstab: no {FILE} line\n{FILE}: absent\nnothing to do\n");
    assert_eq!(again, want);
}

/// Every unit in the list refuses alone, so a shortened list is caught; with
/// `--kubelet-allows-swap` each is a warning and the plan goes on.
#[test]
fn an_active_kubelet_refuses_unless_its_swap_is_allowed() {
    for unit in KUBELETS {
        let (exec, world) = host("swap-kubelet");
        world.borrow_mut().active.insert(unit.to_owned());
        let err = plan(&exec, &swap(FILE), &mut Vec::new()).expect_err(unit);
        assert!(
            matches!(err, HostError::Kubelet(u) if u == unit),
            "{unit}: {err}"
        );
        let a = Swap {
            kubelet_allows_swap: true,
            ..swap(FILE)
        };
        let mut out = Vec::new();
        let p = plan(&exec, &a, &mut out).expect("allowed");
        assert!(
            String::from_utf8(out)
                .expect("utf-8")
                .starts_with(&format!("WARN: {unit} is active"))
        );
        assert_eq!(p.steps().len(), 5);
    }
}

/// Catches a swapfile check that resizes (or accepts) a file of another size, takes a
/// directory for the file, makes a file on a file system `fallocate` cannot serve,
/// or without its size and 16 GiB to spare (exactly that much is refused, one KiB
/// more is enough), or reads a `df` line it does not understand.
#[test]
fn the_swapfile_and_its_file_system_are_checked() {
    type Want = fn(&HostError) -> bool;
    let spare = (GIB + SPARE_GIB * GIB) / 1024;
    type Plant = Box<dyn Fn(&ScratchHost, &RefCell<World>)>;
    let rows: [(&str, Plant, Want); 6] = [
        (
            "2 GiB file",
            Box::new(|e, _| {
                fs::File::create(e.at(FILE))
                    .expect("f")
                    .set_len(2 * GIB)
                    .expect("len");
            }),
            |e| matches!(e, HostError::SwapLen { len, .. } if *len == 2 * GIB),
        ),
        (
            "a directory",
            Box::new(|e, _| fs::create_dir(e.at(FILE)).expect("dir")),
            |e| matches!(e, HostError::NotAFile(_)),
        ),
        (
            "btrfs",
            Box::new(|_, w| w.borrow_mut().fs = "btrfs".to_owned()),
            |e| matches!(e, HostError::FsType { fs, dir } if fs == "btrfs" && dir == "/srv/swap"),
        ),
        (
            "exactly the room",
            Box::new(move |_, w| w.borrow_mut().free_kib = spare),
            |e| matches!(e, HostError::NoSpace { free_gib: 17, .. }),
        ),
        (
            "no free space",
            Box::new(|_, w| w.borrow_mut().free_kib = 0),
            |e| matches!(e, HostError::NoSpace { .. }),
        ),
        (
            "fstab a directory",
            Box::new(|e, _| {
                fs::remove_file(e.at("/etc/fstab")).expect("rm");
                fs::create_dir(e.at("/etc/fstab")).expect("dir");
            }),
            |e| matches!(e, HostError::NotAFile(p) if p.as_str() == "/etc/fstab"),
        ),
    ];
    for (why, plant, want) in rows {
        let (exec, world) = host("swap-refused");
        plant(&exec, &world);
        let err = plan(&exec, &swap(FILE), &mut Vec::new()).expect_err(why);
        assert!(want(&err), "{why}: {err}");
    }
    let (exec, world) = host("swap-room");
    world.borrow_mut().free_kib = spare + 1;
    plan(&exec, &swap(FILE), &mut Vec::new()).expect("one KiB more is enough");
}

/// Catches a `df` answer without its numbers being read as some free space.
#[test]
fn a_df_answer_without_numbers_is_refused() {
    let mut exec = ScratchHost::new("swap-df");
    exec.answer = Box::new(|cmd| {
        Ok(match cmd.program.as_str() {
            FINDMNT => reply(0, "xfs\n", ""),
            DF => reply(0, "Filesystem\n", ""),
            SYSTEMCTL => reply(3, "", ""),
            _ => reply(0, "", ""),
        })
    });
    let err = plan(&exec, &swap(FILE), &mut Vec::new()).expect_err("df");
    assert!(matches!(err, HostError::Output { .. }), "{err}");
}

/// Catches a right-sized file whose mode is not 0600 left as it is, and a default
/// `/swapfile` whose file system is looked up anywhere but at `/`.
#[test]
fn a_readable_file_gets_0600_and_a_root_file_checks_slash() {
    let (exec, world) = host("swap-chmod");
    fs::File::create(exec.at(FILE))
        .expect("f")
        .set_len(GIB)
        .expect("len");
    fs::set_permissions(exec.at(FILE), fs::Permissions::from_mode(0o644)).expect("chmod");
    fs::write(exec.at("/etc/fstab"), format!("{FILE} none swap sw 0 0\n")).expect("fstab");
    world.borrow_mut().swaps.push(FILE.to_owned());
    let p = plan(&exec, &swap(FILE), &mut Vec::new()).expect("plan");
    let steps: Vec<String> = p.steps().iter().map(ToString::to_string).collect();
    assert_eq!(steps, [format!("run /usr/bin/chmod 600 {FILE}")]);

    let (exec, _) = host("swap-slash");
    fs::remove_file(exec.at("/etc/fstab")).expect("rm");
    let p = plan(&exec, &swap("/swapfile"), &mut Vec::new()).expect("plan");
    let asked: Vec<String> = exec
        .queries
        .borrow()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        asked.contains(&"/usr/bin/findmnt -n -o FSTYPE --target /".to_owned()),
        "{asked:?}"
    );
    assert!(asked.contains(&"/usr/bin/df -Pk /".to_owned()), "{asked:?}");
    let last = p.steps().last().expect("steps").to_string();
    assert!(last.starts_with("write /etc/fstab (0644, "), "{last}");
}

/// Catches a size that is not whole GiB, zero, or too large being taken as some size.
#[test]
fn a_swap_size_is_whole_gib() {
    assert_eq!(size_bytes("128G").expect("128G"), 128 * GIB);
    for bad in ["128", "G", "0G", "1.5G", "-1G", "+1G", "1g", "99999999999G"] {
        assert!(
            matches!(size_bytes(bad), Err(HostError::SwapSize(s)) if s == bad),
            "{bad}"
        );
    }
}

/// Catches a tool that cannot start (missing, not executable) read as "no swap".
#[test]
fn a_tool_that_cannot_start_is_an_error() {
    let mut exec = ScratchHost::new("swap-no-tool");
    exec.answer = Box::new(|_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
    let err = plan(&exec, &swap(FILE), &mut Vec::new()).expect_err("no swapon");
    assert!(
        matches!(&err, HostError::Query { cmd, .. } if cmd.starts_with("/usr/sbin/swapon")),
        "{err}"
    );
}

/// Catches an fstab line glued to a last line that has its newline, or a blank line
/// put between them.
#[test]
fn the_fstab_line_follows_a_last_line_with_its_newline() {
    let (exec, _) = host("swap-newline");
    fs::write(exec.at("/etc/fstab"), format!("{OTHER_LINE}\n")).expect("fstab");
    let p = plan(&exec, &swap(FILE), &mut Vec::new()).expect("plan");
    let Some(Step::WriteFile { bytes, .. }) = p.steps().last() else {
        panic!("the last step is not the fstab write: {:?}", p.steps());
    };
    let want = format!("{OTHER_LINE}\n{FILE} none swap sw 0 0\n");
    assert_eq!(String::from_utf8_lossy(bytes), want);
}

/// Catches a rollback that plans a step for what is not there (a `swapoff` of a file
/// that is not active, an fstab rewrite with no line to drop, a removal of no file).
#[test]
fn a_rollback_on_a_host_without_the_swapfile_plans_nothing() {
    // Every tool exits 0 and prints nothing: no swap is active.
    let exec = ScratchHost::new("swap-none");
    let a = Swap {
        remove: true,
        ..swap(FILE)
    };
    assert!(plan(&exec, &a, &mut Vec::new()).expect("plan").is_empty());
}

/// fstab lines that name the swapfile's path without being its line: another file
/// whose name starts with it, and a comment. Adding must still append the line, and
/// `--remove` must drop that line alone. Catches a line matched by the path appearing
/// anywhere in it rather than by its first field (the swapfile would never be listed,
/// and a rollback would drop the look-alikes).
#[test]
fn fstab_lines_that_only_mention_the_swapfile_are_not_its_line() {
    let look_alikes = format!("{FILE}.old none swap sw 0 0\n# {FILE} none swap sw 0 0\n");
    let (mut exec, world) = host("swap-look-alike");
    fs::write(exec.at("/etc/fstab"), &look_alikes).expect("fstab");
    let out = go(&mut exec, &swap(FILE), Mode::Apply);
    assert!(!out.contains("/etc/fstab: has"), "{out}");
    let listed = format!("{look_alikes}{FILE} none swap sw 0 0\n");
    assert_eq!(fstab(&exec), listed);

    let a = Swap {
        remove: true,
        ..swap(FILE)
    };
    go(&mut exec, &a, Mode::Apply);
    assert_eq!(fstab(&exec), look_alikes);
    assert!(world.borrow().swaps.is_empty());

    // With only the look-alikes left, a rollback has no line to drop.
    let again = go(&mut exec, &a, Mode::DryRun);
    assert!(
        again.contains(&format!("/etc/fstab: no {FILE} line\n")),
        "{again}"
    );
}
