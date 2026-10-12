use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::config::Config;
use crate::pilot::config::KEYS;
use kbf_host::Step;
use kbf_host::testkit::{FakeExec, RUNNER, reply};

const EXAMPLE: &str = include_str!("../../templates/pilot/config.example");
const WT: &str = "/srv/wt/kbf-pilot";
const TARGET: &str = "/srv/wt/kbf-pilot/.buckconfig.local";

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("a test path")
}

fn overlay() -> Overlay {
    let c = Config::parse(EXAMPLE, KEYS).expect("valid");
    Overlay {
        settings: OverlaySettings::from_config(&c).expect("valid"),
        worktree: p(WT),
    }
}

/// What git answers about the worktree.
struct Git {
    git_dir: &'static str,
    common: &'static str,
    ignored: bool,
}

fn file(exec: &mut FakeExec, path: &str, bytes: &[u8]) {
    let entry = Entry::File {
        perms: Perms::new(0o644),
        owner: RUNNER,
        bytes: bytes.to_vec(),
    };
    exec.entries.insert(p(path), entry);
}

/// A linked worktree whose .buckconfig.local is gitignored and absent.
fn host() -> (FakeExec, Rc<RefCell<Git>>) {
    let git = Rc::new(RefCell::new(Git {
        git_dir: "/srv/main/.git/worktrees/kbf-pilot",
        common: "/srv/main/.git",
        ignored: true,
    }));
    let mut exec = FakeExec::new(1000);
    let g = Rc::clone(&git);
    exec.answer = Box::new(move |cmd| {
        let g = g.borrow();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        Ok(match (cmd.program.as_str(), &args[..]) {
            (
                GIT,
                [
                    "-C",
                    WT,
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-dir",
                    "--git-common-dir",
                ],
            ) if g.git_dir.is_empty() => reply(128, "", "fatal: not a git repository\n"),
            (
                GIT,
                [
                    "-C",
                    WT,
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-dir",
                    "--git-common-dir",
                ],
            ) => reply(0, &format!("{}\n{}\n", g.git_dir, g.common), ""),
            (GIT, ["-C", WT, "check-ignore", "-q", ".buckconfig.local"]) => {
                reply(i32::from(!g.ignored), "", "")
            }
            (other, _) => panic!("unexpected tool {other} {args:?}"),
        })
    });
    file(&mut exec, &format!("{WT}/.buckconfig"), b"[cells]\n");
    (exec, git)
}

fn planned(exec: &FakeExec, a: &Overlay) -> (Result<Plan, PilotError>, String) {
    let mut out = Vec::new();
    let got = plan(exec, a, &mut out);
    (got, String::from_utf8(out).expect("utf-8"))
}

fn refusal(exec: &FakeExec, a: &Overlay) -> String {
    planned(exec, a)
        .0
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

fn written(plan: &Plan) -> Option<String> {
    plan.steps().iter().find_map(|s| match s {
        Step::WriteFile { path, bytes, .. } if path.as_str() == TARGET => {
            Some(String::from_utf8(bytes.clone()).expect("text"))
        }
        _ => None,
    })
}

/// Catches an overlay that does not start with the marker (the next run, and
/// `--remove`, would then refuse their own file), a value not filled in or put under
/// the wrong key, and a plan that writes when the same overlay is there.
#[test]
fn the_overlay_is_written_with_the_marker_and_every_value_and_then_left_alone() {
    let (mut exec, _) = host();
    let (got, out) = planned(&exec, &overlay());
    let plan = got.expect("plans");
    let text = written(&plan).expect("the overlay is written");
    assert!(text.starts_with(&format!("{MARKER}:")), "{text}");
    for line in [
        "  engine_address = grpc://192.0.2.10:8980",
        "  action_cache_address = grpc://192.0.2.10:8980",
        "  cas_address = grpc://192.0.2.10:8980",
        "  instance_name = main",
        "  tls = false",
        "  execution_concurrency_limit = 8",
        "  linux_x86_64_properties = OSFamily=Linux,label.pool=linux",
        "  darwin_arm64_properties = OSFamily=Darwin,label.pool=mac",
        "  darwin_macos_hosts = 26.0-0123456789abcdef",
    ] {
        assert!(text.lines().any(|l| l == line), "{line} in\n{text}");
    }
    assert_eq!(
        plan.steps()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [format!("write {TARGET} (0644, {} bytes)", text.len())]
    );
    assert!(
        out.starts_with(&format!("{TARGET}: writes\n    | {MARKER}:")),
        "{out}"
    );
    assert!(
        out.contains("WARN: the linux actions of this worktree go to kbf too"),
        "{out}"
    );
    execute(&plan, RunAs::User, Mode::Apply, &mut exec, &mut Vec::new()).expect("applies");
    let (again, out) = planned(&exec, &overlay());
    assert!(again.expect("plans").is_empty());
    assert_eq!(out, format!("{TARGET}: unchanged\n"));
    let mut changed = overlay();
    changed.settings.concurrency = "16".into();
    let rewritten = written(&planned(&exec, &changed).0.expect("plans")).expect("rewritten");
    assert!(rewritten.contains("execution_concurrency_limit = 16"));
}

/// The planted defect this verb exists to refuse: **buckconfig overwrites a foreign
/// file.** A `.buckconfig.local` the operator wrote (no marker on its first line), a
/// marker that is not at the start, a link and a directory are each refused, with
/// `--apply` as without, and the file is left exactly as it was: no step is planned
/// or applied.
#[test]
fn a_foreign_buckconfig_local_is_never_overwritten() {
    let foreign: [(&str, Entry); 4] = [
        (
            "operator's own",
            Entry::File {
                perms: Perms::new(0o644),
                owner: RUNNER,
                bytes: b"[buck2_re_client]\n  engine_address = grpc://elsewhere:1\n".to_vec(),
            },
        ),
        (
            "marker on line 2",
            Entry::File {
                perms: Perms::new(0o644),
                owner: RUNNER,
                bytes: format!("# mine\n{MARKER}\n").into_bytes(),
            },
        ),
        ("a link", Entry::Other),
        (
            "a directory",
            Entry::Dir {
                perms: Perms::new(0o755),
                owner: RUNNER,
            },
        ),
    ];
    for (what, entry) in foreign {
        let (mut exec, _) = host();
        exec.entries.insert(p(TARGET), entry.clone());
        let want = format!(
            "{TARGET} was not written by this verb (its first line is not '{MARKER}'); move it aside first. Left alone"
        );
        assert_eq!(refusal(&exec, &overlay()), want, "{what}");
        let mut out = Vec::new();
        let applied = run(&mut exec, &overlay(), Mode::Apply, &mut out)
            .err()
            .map(|e| e.to_string());
        assert_eq!(applied.as_deref(), Some(want.as_str()), "{what}");
        let removed = run_remove(&mut exec, &p(WT), Mode::Apply, &mut out)
            .err()
            .map(|e| e.to_string());
        assert_eq!(removed.as_deref(), Some(want.as_str()), "{what}");
        assert!(exec.log.is_empty(), "{what}: {:?}", exec.log);
        assert_eq!(exec.entries.get(&p(TARGET)), Some(&entry), "{what}");
    }
}

/// Catches `--remove` deleting a file it did not write (above), leaving its own
/// overlay, or failing when there is none.
#[test]
fn remove_deletes_only_an_overlay_this_verb_wrote() {
    let (mut exec, _) = host();
    file(&mut exec, TARGET, format!("{MARKER}: old\n").as_bytes());
    let mut out = Vec::new();
    run_remove(&mut exec, &p(WT), Mode::Apply, &mut out).expect("removes");
    assert_eq!(exec.log, [Step::Remove { path: p(TARGET) }]);
    assert!(!exec.entries.contains_key(&p(TARGET)));
    let mut out = Vec::new();
    let plan = plan_remove(&exec, &p(WT), &mut out).expect("plans");
    assert!(plan.is_empty());
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!("no {TARGET}\n")
    );
}

/// Catches an overlay written into a checkout that is not a buck2 one, into the main
/// checkout (every other session builds from it), or where `.buckconfig.local` is not
/// gitignored (one `git add` would commit the pilot's address).
#[test]
fn a_main_checkout_a_non_checkout_or_an_unignored_overlay_is_refused() {
    let (mut exec, _) = host();
    exec.entries.remove(&p(&format!("{WT}/.buckconfig")));
    assert_eq!(
        refusal(&exec, &overlay()),
        format!("{WT} has no .buckconfig: not a buck2 checkout")
    );
    exec.entries
        .insert(p(&format!("{WT}/.buckconfig")), Entry::Other);
    assert_eq!(
        refusal(&exec, &overlay()),
        format!("{WT} has no .buckconfig: not a buck2 checkout")
    );

    let (exec, git) = host();
    git.borrow_mut().git_dir = "/srv/main/.git";
    let main = format!("{WT} is the main checkout; write the overlay into a linked worktree only");
    assert_eq!(refusal(&exec, &overlay()), main);
    git.borrow_mut().git_dir = "";
    git.borrow_mut().common = "";
    assert_eq!(
        refusal(&exec, &overlay()),
        main,
        "not a git checkout: git printed nothing"
    );

    let (exec, git) = host();
    git.borrow_mut().ignored = false;
    assert_eq!(
        refusal(&exec, &overlay()),
        format!(".buckconfig.local is not gitignored in {WT}")
    );
    assert!(exec.log.is_empty());
}

/// Catches a property set kbf would not route as meant written to the overlay: no
/// OSFamily (it matches every node), two, another OS's, a malformed pair, a darwin
/// label no daemon carries, the two sets the same, and a value the overlay cannot
/// carry; and a check that refuses the spellings kbf takes.
#[test]
fn each_bad_property_set_is_refused() {
    type Plant = fn(&mut OverlaySettings);
    let cases: [(Plant, &str); 9] = [
        (
            |s| s.linux_properties = "label.pool=linux".into(),
            "KBF_LINUX_PROPERTIES (label.pool=linux) names no OSFamily: kbf ignores names it does not know, so it would match every node. Add OSFamily=Linux",
        ),
        (
            |s| s.darwin_properties = "pool=mac".into(),
            "KBF_DARWIN_PROPERTIES (pool=mac) names no OSFamily: kbf ignores names it does not know, so it would match every node. Add OSFamily=Darwin",
        ),
        (
            |s| s.linux_properties = "OSFamily=Linux,osfamily=linux".into(),
            "KBF_LINUX_PROPERTIES names OSFamily 2 times (OSFamily=Linux,osfamily=linux); kbf refuses that",
        ),
        (
            |s| s.linux_properties = "OSFamily=Darwin".into(),
            "KBF_LINUX_PROPERTIES: OSFamily=Darwin does not name linux",
        ),
        (
            |s| s.darwin_properties = "OSFamily=Windows".into(),
            "KBF_DARWIN_PROPERTIES: OSFamily=Windows does not name darwin",
        ),
        (
            |s| s.linux_properties = "OSFamily=Linux,pool".into(),
            "KBF_LINUX_PROPERTIES: property 'pool' is not KEY=VALUE",
        ),
        (
            |s| s.darwin_properties = "OSFamily=Darwin,label.pool=big".into(),
            "KBF_DARWIN_PROPERTIES asks for label.pool=big, but no daemon carries --label pool=big (KBF_DARWIN_LABELS=pool=mac)",
        ),
        (
            |s| s.darwin_properties = s.linux_properties.clone(),
            "the linux and darwin property sets must differ",
        ),
        (
            |s| s.darwin_macos_hosts = "26.0-a $(x)".into(),
            "KBF_DARWIN_MACOS_HOSTS holds a character the overlay cannot carry: \"26.0-a $(x)\"",
        ),
    ];
    for (plant, want) in cases {
        let (exec, _) = host();
        let mut a = overlay();
        plant(&mut a.settings);
        assert_eq!(refusal(&exec, &a), want);
        assert!(
            exec.queries.borrow().is_empty(),
            "{want}: refused before asking git"
        );
    }
    for darwin in ["darwin", "Darwin", "MACOS", "macosx", "osx"] {
        assert!(
            routes_to(Os::Darwin, &format!("osFamily={darwin}")).is_ok(),
            "{darwin}"
        );
    }
    assert!(routes_to(Os::Linux, "OSFAMILY=LINUX,ISA=x86-64").is_ok());
    assert!(
        labels_carried(
            "OSFamily=Darwin,LABEL.pool=mac,label.x=1",
            "a=b,pool=mac,x=1"
        )
        .is_ok()
    );
    let bad = ["=x", "x=", "a=b=c", "a b=c"];
    for pair in bad {
        assert!(pair_of("K", pair).is_err(), "{pair}");
    }
    assert!(labels_carried("pool", "").is_err());
}
