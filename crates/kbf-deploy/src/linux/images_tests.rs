use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use super::*;
use crate::linux::config::image;
use kbf_host::testkit::{FakeExec, reply};
use kbf_host::user::GETENT;

const A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const EXAMPLE: &str = include_str!("../../templates/linux/config.example");

fn ref_a() -> String {
    format!("registry.example:5000/kbf/build@{A}")
}

fn ref_b() -> String {
    format!("gcr.io/distroless/base-debian12@{B}")
}

fn config(images: &str) -> LinuxConfig {
    let text = EXAMPLE.replace("KBF_IMAGES=\n", &format!("KBF_IMAGES={images}\n"));
    LinuxConfig::parse(&text).expect("valid settings")
}

/// What the fake store holds: image -> `<digest> <os>/<arch>`.
type Store = Rc<RefCell<BTreeMap<String, String>>>;

/// A root host with user kbf (990) whose Podman store is `store`; `podman pull` adds
/// the image with `pulled` as its inspect line (its own digest when `None`).
fn host(store: &Store, pulled: Option<&'static str>) -> FakeExec {
    let mut exec = FakeExec::new(0);
    let store = Rc::clone(store);
    exec.answer = Box::new(move |cmd| {
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let mut store = store.borrow_mut();
        Ok(match (cmd.program.as_str(), &args[..]) {
            (GETENT, _) => reply(0, "kbf:x:990:990::/var/lib/kbf:/usr/sbin/nologin\n", ""),
            (PODMAN, ["image", "exists", image]) => {
                reply(i32::from(!store.contains_key(*image)), "", "")
            }
            (PODMAN, ["pull", "--quiet", image]) => {
                let digest = image.rsplit('@').next().expect("pinned");
                let line = pulled.map_or(format!("{digest} linux/amd64"), str::to_owned);
                store.insert((*image).to_owned(), line);
                reply(0, "", "")
            }
            (PODMAN, ["image", "inspect", "--format", _, image]) => reply(0, &store[*image], ""),
            _ => reply(0, "990 44\n", ""),
        })
    });
    exec
}

fn pull(c: LinuxConfig, images: &[String]) -> Pull {
    Pull {
        config: c,
        images: images.iter().map(|i| image(i).expect("valid")).collect(),
        platform: PLATFORM.to_owned(),
    }
}

fn text(out: Vec<u8>) -> String {
    String::from_utf8(out).expect("utf-8")
}

/// The plan's test: a dry run pulls nothing and prints the pull of each missing image,
/// as the kbf user with its Podman environment; the apply pulls it and checks it; a
/// re-run plans nothing. Catches a pull of an image already there, a pull or read run
/// as root (into root's store), and a verb that does not re-check after its pulls.
#[test]
fn a_dry_run_names_each_missing_pull_and_a_rerun_after_apply_pulls_nothing() {
    let store: Store = Rc::default();
    store
        .borrow_mut()
        .insert(ref_a(), format!("{A} linux/amd64"));
    let mut exec = host(&store, None);
    let a = pull(config(&format!("{} {}", ref_a(), ref_b())), &[]);

    let mut out = Vec::new();
    run(&mut exec, &a, Mode::DryRun, &mut out).expect("dry run");
    let want = format!(
        "{}: in kbf's store, linux/amd64 and digest checked\n\
         1 image(s) to pull; each is checked after the pull\n\
         DRY-RUN: run (as kbf) /usr/bin/podman pull --quiet {} \
         (env HOME, USER, LOGNAME, XDG_RUNTIME_DIR, PATH)\n\
         dry run: nothing changed. Re-run with --apply.\n",
        ref_a(),
        ref_b()
    );
    assert_eq!(text(out), want);
    assert!(exec.log.is_empty() && store.borrow().len() == 1);
    for q in exec
        .queries
        .borrow()
        .iter()
        .filter(|q| q.program.as_str() == PODMAN)
    {
        let user = q.user.as_ref().expect("podman runs as kbf");
        assert_eq!(
            (user.uid, user.gid, &user.groups[..]),
            (990, 990, &[990, 44][..])
        );
        let env: BTreeMap<_, _> = q.env.iter().cloned().collect();
        assert_eq!(env["XDG_RUNTIME_DIR"], "/run/user/990");
        assert_eq!(env["HOME"], "/var/lib/kbf");
    }

    let mut out = Vec::new();
    run(&mut exec, &a, Mode::Apply, &mut out).expect("apply");
    assert_eq!(exec.log.len(), 1);
    assert!(store.borrow().contains_key(&ref_b()));
    let checked = format!(
        "{}: in kbf's store, linux/amd64 and digest checked",
        ref_b()
    );
    assert!(text(out).contains(&checked), "the pull was not checked");

    let mut out = Vec::new();
    run(&mut exec, &a, Mode::Apply, &mut out).expect("re-run");
    assert!(text(out).ends_with("nothing to do\n"));
    assert_eq!(
        exec.log.len(),
        1,
        "an apply with nothing to pull ran a step"
    );
}

/// Catches `--image` being added to `KBF_IMAGES` instead of replacing it.
#[test]
fn an_image_flag_replaces_the_configured_images() {
    let store: Store = Rc::default();
    let exec = host(&store, None);
    let a = pull(config(&ref_a()), &[ref_b()]);
    let plan = plan(&exec, &a, &mut Vec::new()).expect("plan");
    assert_eq!(plan.steps().len(), 1);
    assert!(plan.steps()[0].to_string().ends_with(&format!(
        "{} (env HOME, USER, LOGNAME, XDG_RUNTIME_DIR, PATH)",
        ref_b()
    )));
}

/// Catches a store copy that is not the pinned image being accepted: an index digest
/// (another digest than the pin), another platform, or an inspect line the verb
/// cannot read; and a pull that leaves the image missing passing as done.
#[test]
fn a_store_copy_that_is_not_the_pin_is_refused() {
    type Want = fn(&HostError) -> bool;
    let rows: [(&str, &str, Want); 3] = [
        ("index digest", "sha256:cccc linux/amd64", |e| {
            matches!(e, HostError::WrongDigest { .. })
        }),
        (
            "arm image",
            "",
            |e| matches!(e, HostError::WrongPlatform { got, .. } if got == "linux/arm64"),
        ),
        ("unreadable line", "garbage", |e| {
            matches!(e, HostError::Output { .. })
        }),
    ];
    for (why, line, want) in rows {
        let line = if line.is_empty() {
            format!("{A} linux/arm64")
        } else {
            line.to_owned()
        };
        let store: Store = Rc::default();
        store.borrow_mut().insert(ref_a(), line);
        let exec = host(&store, None);
        let err = plan(&exec, &pull(config(&ref_a()), &[]), &mut Vec::new()).expect_err(why);
        assert!(want(&err), "{why}: {err}");
    }

    // A pull whose result is another digest is refused when the apply re-checks it.
    let store: Store = Rc::default();
    let mut exec = host(&store, Some("sha256:dddd linux/amd64"));
    let err = run(
        &mut exec,
        &pull(config(&ref_a()), &[]),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("re-check");
    assert!(
        matches!(&err, HostError::AfterApply(e) if matches!(**e, HostError::WrongDigest { .. })),
        "{err}"
    );
}

/// Catches an apply that reports success when `podman pull` exited 0 but the image is
/// still not in the store.
#[test]
fn a_pull_that_leaves_the_image_missing_fails() {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        Ok(match cmd.program.as_str() {
            GETENT => reply(0, "kbf:x:990:990::/h:/s\n", ""),
            PODMAN if cmd.args[0] == "image" => reply(1, "", ""),
            _ => reply(0, "990\n", ""),
        })
    });
    let err = run(
        &mut exec,
        &pull(config(&ref_a()), &[]),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("still missing");
    assert!(
        matches!(&err, HostError::AfterApply(e) if matches!(&**e, HostError::NotPulled(i) if *i == ref_a())),
        "{err}"
    );
}

/// Catches a verb that runs with no image, reads the store as someone other than root
/// (the switch to the kbf user needs root, even for a dry run) or for a user that does
/// not exist.
#[test]
fn image_pull_refuses_no_image_no_root_and_no_user() {
    let store: Store = Rc::default();
    let exec = host(&store, None);
    let err = plan(&exec, &pull(config(""), &[]), &mut Vec::new()).expect_err("no image");
    assert!(matches!(err, HostError::NoImages), "{err}");

    let mut user = host(&store, None);
    user.euid = 501;
    let err = plan(&user, &pull(config(&ref_a()), &[]), &mut Vec::new()).expect_err("user");
    assert!(matches!(err, HostError::PullNeedsRoot(_)), "{err}");
    assert!(user.queries.borrow().is_empty(), "it asked the host first");

    let mut nobody = FakeExec::new(0);
    nobody.answer = Box::new(|_| Ok(reply(2, "", "")));
    let err = plan(&nobody, &pull(config(&ref_a()), &[]), &mut Vec::new()).expect_err("no kbf");
    assert!(matches!(&err, HostError::NoUser(u) if u == "kbf"), "{err}");
}
