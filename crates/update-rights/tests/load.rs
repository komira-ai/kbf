//! Loading a directory of declarations: who may have written them, what each must keep, and
//! what two of them may not both claim.

mod support;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use support::{Scratch, accounts, my_gid, my_uid};
use update_hook::Step;
use update_rights::reserved::Violation;
use update_rights::{
    Digest, EtcAccounts, Finished, Launch, Loaded, Policy, Refusal, Spawner, SystemdRun, UserIds,
    check_state_dir, load_dir, recheck_targets, run_step,
};

/// One declaration, small enough to vary: the account names, the claims that can collide, the
/// paths.
struct Spec {
    name: String,
    uid: String,
    hook_user: String,
    ports: Vec<u16>,
    units: Vec<String>,
    state_dir: String,
    read: Vec<String>,
    write: Vec<String>,
}

impl Spec {
    fn new(name: &str) -> Spec {
        Spec {
            name: name.to_owned(),
            uid: name.to_owned(),
            hook_user: format!("{name}-update"),
            ports: Vec::new(),
            units: vec![format!("{name}.service")],
            state_dir: format!("/var/lib/updater/hooks/{name}"),
            read: Vec::new(),
            write: Vec::new(),
        }
    }

    fn list<T: std::fmt::Display>(items: &[T]) -> String {
        let quoted: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
        format!("[{}]", quoted.join(", "))
    }

    fn text(&self) -> String {
        let ports: Vec<String> = self.ports.iter().map(u16::to_string).collect();
        format!(
            r#"name = "{name}"
binary = "{name}"
signer = {{ repository = "komira-ai/kbf", workflow = "artifacts.yml" }}
uid = "{uid}"
health_ports = [{ports}]
verify_deadline_secs = 20
state_dir = "{state}"
[hook]
user = "{hook}"
group = "{hook}"
read_paths = {read}
write_paths = {write}
units = {units}
[hook.timeout_secs]
prepare = 1
cutover = 1
verify = 10
rollback = 10
cleanup = 1
"#,
            name = self.name,
            uid = self.uid,
            ports = ports.join(", "),
            state = self.state_dir,
            hook = self.hook_user,
            read = Spec::list(&self.read),
            write = Spec::list(&self.write),
            units = Spec::list(&self.units),
        )
    }
}

/// A directory of declarations owned by the test's own account, which is the trusted one.
struct Host {
    scratch: Scratch,
    dir: PathBuf,
}

impl Host {
    fn new(name: &str) -> Host {
        let scratch = Scratch::new(name);
        let dir = scratch.dir("updater.d", 0o755);
        Host { scratch, dir }
    }

    fn put(&self, spec: &Spec) -> PathBuf {
        self.scratch_file(&format!("{}.toml", spec.name), &spec.text())
    }

    fn scratch_file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        path
    }

    fn load(&self) -> Loaded {
        self.load_with(&accounts())
    }

    fn load_with(&self, accounts: &EtcAccounts) -> Loaded {
        let policy = Policy {
            trusted_uid: my_uid(),
            accounts,
        };
        load_dir(&self.dir, &policy).expect("the directory lists")
    }
}

fn refused(loaded: &Loaded, service: &str) -> Refusal {
    let found: Vec<_> = loaded
        .refused
        .iter()
        .filter(|r| r.service == service)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "{service} must be refused once: {:?}",
        loaded.refused
    );
    found[0].reason.clone()
}

fn account_text(rows: &str) -> EtcAccounts {
    EtcAccounts::new(rows, "")
}

/// Catches a loader that does not return what it read: every service, its digest (of the exact
/// bytes), its file and the ids of its account and groups.
#[test]
fn a_good_directory_loads_every_service() {
    let host = Host::new("good");
    let tierstore = Spec::new("tierstore");
    let path = host.put(&tierstore);
    host.put(&Spec::new("other"));
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
    assert_eq!(
        loaded
            .services
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["other", "tierstore"]
    );
    let declared = &loaded.services["tierstore"];
    assert_eq!(declared.digest, Digest::of(tierstore.text().as_bytes()));
    assert_eq!(declared.file, path);
    assert_eq!(declared.service, UserIds { uid: 990, gid: 990 });
    assert_eq!(declared.service_gids, [994, 990]);
    assert_eq!(declared.declaration.name, "tierstore");
}

/// Catches files that are not declarations being read as declarations: the approval record
/// beside them, notes, and a file whose name is not UTF-8 text ending `.toml`.
#[test]
fn only_toml_files_are_declarations() {
    let host = Host::new("ignored");
    host.put(&Spec::new("tierstore"));
    host.scratch_file("approved", "not a declaration");
    host.scratch_file("notes.txt", "not a declaration");
    host.scratch_file("tierstore.toml.bak", "not a declaration");
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
    assert_eq!(loaded.services.len(), 1);
}

/// Catches a bad declaration refusing its neighbours, or being loaded in spite of its fault:
/// the service with the bad file is refused and the good one loads.
#[test]
fn a_bad_declaration_refuses_only_its_own_service() {
    let host = Host::new("bad-beside-good");
    host.put(&Spec::new("tierstore"));
    host.scratch_file("other.toml", "name = [");
    let loaded = host.load();
    assert_eq!(loaded.services.keys().collect::<Vec<_>>(), ["tierstore"]);
    assert!(matches!(refused(&loaded, "other"), Refusal::Syntax(_)));
    assert!(loaded.refused[0].file.ends_with("other.toml"));
}

/// Catches an empty or missing directory being read as trouble or as an unsafe one.
#[test]
fn an_empty_directory_loads_nothing_and_a_missing_one_is_an_error() {
    let host = Host::new("empty");
    let loaded = host.load();
    assert!(loaded.services.is_empty() && loaded.refused.is_empty());
    let policy = Policy {
        trusted_uid: my_uid(),
        accounts: &accounts(),
    };
    assert!(load_dir(&host.dir.join("missing"), &policy).is_err());
}

/// Catches a file another account could have changed being trusted: a file the group or others
/// can write, a link, a directory named like a declaration.
#[test]
fn a_file_that_others_can_write_or_that_is_not_a_file_is_refused() {
    let host = Host::new("file-trust");
    for (name, mode) in [("group", 0o664), ("world", 0o666), ("both", 0o622)] {
        let spec = Spec::new(name);
        let path = host.put(&spec);
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
    }
    let target = host.put(&Spec::new("target"));
    symlink(&target, host.dir.join("link.toml")).unwrap();
    fs::create_dir(host.dir.join("dir.toml")).unwrap();
    let loaded = host.load_with(&account_text(""));
    for service in ["group", "world", "both"] {
        match refused(&loaded, service) {
            Refusal::File(why) => assert!(why.contains("writ"), "{service}: {why}"),
            other => panic!("{service}: {other:?}"),
        }
    }
    for service in ["link", "dir"] {
        assert!(
            matches!(refused(&loaded, service), Refusal::File(_)),
            "{service}"
        );
    }
}

/// Catches a file owned by an account that is not the trusted one: here the trusted account is
/// not the one that made the file.
#[test]
fn a_directory_not_owned_by_the_trusted_account_refuses_every_file() {
    let host = Host::new("dir-owner");
    host.put(&Spec::new("tierstore"));
    host.put(&Spec::new("other"));
    let policy = Policy {
        trusted_uid: my_uid() + 1,
        accounts: &accounts(),
    };
    let loaded = load_dir(&host.dir, &policy).unwrap();
    assert!(loaded.services.is_empty());
    for service in ["tierstore", "other"] {
        assert!(
            matches!(refused(&loaded, service), Refusal::DirectoryUnsafe(_)),
            "{service}"
        );
    }
}

/// Catches a directory that others can write, or that is a link, being trusted: its files can be
/// replaced by anyone who can write it.
#[test]
fn a_directory_others_can_write_refuses_every_file() {
    for mode in [0o775, 0o757, 0o777] {
        let host = Host::new("dir-mode");
        host.put(&Spec::new("tierstore"));
        fs::set_permissions(
            &host.dir,
            std::os::unix::fs::PermissionsExt::from_mode(mode),
        )
        .unwrap();
        let loaded = host.load();
        assert!(loaded.services.is_empty(), "{mode:o}");
        match refused(&loaded, "tierstore") {
            Refusal::DirectoryUnsafe(why) => assert!(why.contains("writ"), "{why}"),
            other => panic!("{mode:o}: {other:?}"),
        }
    }
    let host = Host::new("dir-link");
    host.put(&Spec::new("tierstore"));
    let link = host.scratch.path().join("link.d");
    symlink(&host.dir, &link).unwrap();
    let policy = Policy {
        trusted_uid: my_uid(),
        accounts: &accounts(),
    };
    let loaded = load_dir(&link, &policy).unwrap();
    assert!(matches!(
        refused(&loaded, "tierstore"),
        Refusal::DirectoryUnsafe(_)
    ));
}

/// Catches a text that is not valid UTF-8 being decoded loosely.
#[test]
fn a_file_that_is_not_text_is_refused() {
    let host = Host::new("utf8");
    let path = host.scratch_file("tierstore.toml", "");
    fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
    let loaded = host.load();
    assert_eq!(
        refused(&loaded, "tierstore"),
        Refusal::Syntax("not UTF-8".to_owned())
    );
}

/// Catches a declaration in a file named for another service: the name in the file is what the
/// updater would act on.
#[test]
fn the_file_is_named_for_the_service() {
    let host = Host::new("name");
    host.scratch_file("other.toml", &Spec::new("tierstore").text());
    let loaded = host.load();
    match refused(&loaded, "other") {
        Refusal::Value { field, .. } => assert_eq!(field, "name"),
        other => panic!("{other:?}"),
    }
}

/// Catches a rule of the text alone being skipped by the loader.
#[test]
fn the_rules_of_the_text_apply_to_a_file() {
    let host = Host::new("text-rules");
    let mut spec = Spec::new("tierstore");
    spec.read = vec!["/etc".to_owned()];
    host.put(&spec);
    let loaded = host.load();
    assert!(matches!(
        refused(&loaded, "tierstore"),
        Refusal::Reserved { .. }
    ));
}

/// Catches an account that does not exist being accepted, whichever of the three names it is.
#[test]
fn the_accounts_must_exist() {
    let host = Host::new("accounts");
    let mut no_uid = Spec::new("tierstore");
    no_uid.uid = "nobody-here".to_owned();
    host.put(&no_uid);
    let mut no_user = Spec::new("other");
    no_user.hook_user = "missing-update".to_owned();
    host.put(&no_user);
    // `third` has an account and a hook user, but its group is not in the group file.
    host.put(&Spec::new("third"));
    let people = account_text("third:x:1:1\nthird-update:x:2:2\nother:x:3:3\nroot:x:0:0\n");
    let loaded = host.load_with(&people);
    assert!(loaded.services.is_empty(), "{:?}", loaded.services.keys());
    for (service, word) in [
        ("tierstore", "uid"),
        ("other", "hook.user"),
        ("third", "hook.group"),
    ] {
        match refused(&loaded, service) {
            Refusal::Account(text) => assert!(text.contains(word), "{service}: {text}"),
            other => panic!("{service}: {other:?}"),
        }
    }
}

/// Catches a hook that runs as the service's own account, which could then write the record
/// the hook trusts: by its name, and by another name for the same uid.
#[test]
fn the_hook_account_is_not_the_service_account() {
    for hook_user in ["tierstore", "alias"] {
        let host = Host::new("hook-user");
        let mut spec = Spec::new("tierstore");
        spec.hook_user = hook_user.to_owned();
        host.put(&spec);
        let loaded = host.load();
        match refused(&loaded, "tierstore") {
            Refusal::Account(text) => assert!(text.contains("hook.user"), "{text}"),
            other => panic!("{hook_user}: {other:?}"),
        }
    }
}

/// Catches root being refused as a hook account (the kbf and js-agent hooks run as root once
/// approved), and a root service being refused for what root can always do.
#[test]
fn a_root_hook_and_a_root_service_are_loaded() {
    let host = Host::new("hook-root");
    let mut root_hook = Spec::new("tierstore");
    root_hook.hook_user = "root".to_owned();
    host.put(&root_hook);
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
    let host = Host::new("both-root");
    let mut both = Spec::new("tierstore");
    both.uid = "root".to_owned();
    both.hook_user = "root".to_owned();
    host.put(&both);
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
    assert_eq!(loaded.services.len(), 1);
}

/// Catches a path that passes the text rules and resolves to a reserved place: a link in a
/// directory of the service's own that points at `/etc`.
#[test]
fn a_link_to_a_reserved_place_is_refused_in_each_kind() {
    let host = Host::new("links");
    let etc = host.scratch.path().join("etc-link");
    symlink("/etc", &etc).unwrap();
    // A write path cannot be tried here: it must lie under `/var/lib`, `/var/cache`, `/run` or
    // `/opt`, where a test without root cannot make a link (the root job does).
    let usr = host.scratch.path().join("usr-link");
    symlink("/usr/lib", &usr).unwrap();
    for (field, read, write, state) in [
        ("hook.read_paths", Some(&etc), None, None),
        ("hook.read_paths", Some(&usr), None, None),
        ("state_dir", None, None, Some(&etc)),
    ] {
        let mut spec = Spec::new("tierstore");
        spec.read = read.map(|p| p.display().to_string()).into_iter().collect();
        spec.write = write
            .map(|p: &std::path::PathBuf| p.display().to_string())
            .into_iter()
            .collect();
        spec.state_dir = state.map_or(spec.state_dir.clone(), |p| p.display().to_string());
        host.put(&spec);
        let loaded = host.load();
        match refused(&loaded, "tierstore") {
            Refusal::Reserved {
                field: found,
                path,
                why,
            } => {
                assert_eq!(found, field);
                assert!(path.contains("which is /"), "{path}");
                assert!(
                    matches!(
                        why,
                        Violation::Names("/etc") | Violation::Contains(_) | Violation::Inside(_)
                    ),
                    "{why:?}"
                );
            }
            other => panic!("{field}: {other:?}"),
        }
    }
}

/// Catches an existing path that resolves somewhere harmless being refused with it.
#[test]
fn a_link_to_an_ordinary_place_is_fine() {
    let host = Host::new("link-fine");
    let real = host.scratch.dir("real", 0o755);
    let link = host.scratch.path().join("link");
    symlink(&real, &link).unwrap();
    let mut spec = Spec::new("tierstore");
    spec.read = vec![link.display().to_string()];
    host.put(&spec);
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
}

fn with_service_account(uid: u32, gid: u32) -> EtcAccounts {
    EtcAccounts::new(
        &format!("root:x:0:0\nsvc:x:{uid}:{gid}\nsvc-update:x:991:991\n"),
        "root:x:0:\nsvc-update:x:991:\n",
    )
}

fn state_spec(state: &Path) -> Spec {
    let mut spec = Spec::new("svc");
    spec.state_dir = state.display().to_string();
    spec
}

/// Catches a record directory the service can write being accepted, by owner: the directory the
/// service's account owns is its own.
#[test]
fn a_state_directory_the_service_owns_is_refused() {
    let host = Host::new("state-owner");
    let state = host.scratch.dir("state", 0o755);
    host.put(&state_spec(&state));
    let loaded = host.load_with(&with_service_account(my_uid(), 990));
    match refused(&loaded, "svc") {
        Refusal::StateDirWritable { path } => assert_eq!(path, state.display().to_string()),
        other => panic!("{other:?}"),
    }
}

/// Catches the group being forgotten: a record directory the service's group can write is the
/// service's to write (3.4.1), and a group that merely owns it, without write, is not.
#[test]
fn a_state_directory_the_services_group_can_write_is_refused() {
    let host = Host::new("state-group");
    let state = host.scratch.dir("state", 0o775);
    host.put(&state_spec(&state));
    let loaded = host.load_with(&with_service_account(990, my_gid()));
    assert!(matches!(
        refused(&loaded, "svc"),
        Refusal::StateDirWritable { .. }
    ));
    // The same directory, group read-only: the service may read and traverse it, not write.
    let host = Host::new("state-group-read");
    let state = host.scratch.dir("state", 0o750);
    host.put(&state_spec(&state));
    let loaded = host.load_with(&with_service_account(990, my_gid()));
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
}

/// Catches a supplementary group being forgotten: the group file lists the service as a member.
#[test]
fn a_state_directory_a_supplementary_group_can_write_is_refused() {
    let host = Host::new("state-member");
    let state = host.scratch.dir("state", 0o775);
    host.put(&state_spec(&state));
    let people = EtcAccounts::new(
        "root:x:0:0\nsvc:x:990:990\nsvc-update:x:991:991\n",
        &format!("root:x:0:\nsvc-update:x:991:\nhelpers:x:{}:svc\n", my_gid()),
    );
    let loaded = host.load_with(&people);
    assert!(matches!(
        refused(&loaded, "svc"),
        Refusal::StateDirWritable { .. }
    ));
}

/// Catches others' write permission being forgotten.
#[test]
fn a_state_directory_anyone_can_write_is_refused() {
    let host = Host::new("state-world");
    let state = host.scratch.dir("state", 0o757);
    host.put(&state_spec(&state));
    let loaded = host.load_with(&with_service_account(990, 990));
    assert!(matches!(
        refused(&loaded, "svc"),
        Refusal::StateDirWritable { .. }
    ));
}

/// Catches a record directory that does not exist yet being judged by nothing: the directory
/// above it, which the service could replace it through, is what counts.
#[test]
fn a_directory_above_the_state_directory_the_service_can_write_is_refused() {
    let host = Host::new("state-parent");
    let parent = host.scratch.dir("parent", 0o777);
    host.put(&state_spec(&parent.join("later")));
    let loaded = host.load_with(&with_service_account(990, 990));
    match refused(&loaded, "svc") {
        Refusal::StateDirWritable { path } => assert_eq!(path, parent.display().to_string()),
        other => panic!("{other:?}"),
    }
    // The same, with nothing writable above it.
    let host = Host::new("state-parent-ok");
    let parent = host.scratch.dir("parent", 0o755);
    host.put(&state_spec(&parent.join("later")));
    let loaded = host.load_with(&with_service_account(990, 990));
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
}

/// Catches the check being made for a service that runs as root, where it can never hold: root
/// can write anything, so the loader does not refuse every root service.
#[test]
fn a_root_service_is_not_refused_for_what_root_can_always_do() {
    let scratch = Scratch::new("state-root");
    let open = scratch.dir("open", 0o777);
    assert_eq!(
        check_state_dir(&open.display().to_string(), 0, &[0]),
        Ok(())
    );
    assert!(matches!(
        check_state_dir(&open.display().to_string(), 990, &[990]),
        Err(Refusal::StateDirWritable { .. })
    ));
}

fn collide(first: impl FnOnce(&mut Spec), second: impl FnOnce(&mut Spec)) -> Loaded {
    let host = Host::new("collide");
    let mut a = Spec::new("tierstore");
    let mut b = Spec::new("other");
    first(&mut a);
    second(&mut b);
    host.put(&a);
    host.put(&b);
    host.put(&Spec::new("tsb"));
    host.load()
}

fn conflict(loaded: &Loaded, service: &str) -> (&'static str, String) {
    match refused(loaded, service) {
        Refusal::Conflict { what, with } => (what, with),
        other => panic!("{service}: {other:?}"),
    }
}

/// Catches two declarations that share what only one may have: the account the registration is
/// told apart by, a unit, a health port, a record directory, a path into it. Both are refused,
/// and a third that claims none of it loads.
#[test]
fn declarations_that_claim_the_same_thing_are_both_refused() {
    type Change = fn(&mut Spec);
    let cases: [(&str, Change, Change); 6] = [
        (
            "the service's account",
            |_| {},
            |b| b.uid = "alias".to_owned(),
        ),
        (
            "a health port",
            |a| a.ports = vec![9301, 9302],
            |b| b.ports = vec![9302, 9303],
        ),
        (
            "the record directory",
            |a| a.state_dir = "/var/lib/updater/hooks/shared".to_owned(),
            |b| b.state_dir = "/var/lib/updater/hooks/shared".to_owned(),
        ),
        (
            "the record directory",
            |a| a.state_dir = "/var/lib/updater/hooks/shared".to_owned(),
            |b| b.state_dir = "/var/lib/updater/hooks/shared/inner".to_owned(),
        ),
        (
            "a write path into the record directory",
            |a| a.write = vec!["/var/lib/updater/hooks/other/work".to_owned()],
            |_| {},
        ),
        (
            "a write path into the record directory",
            |_| {},
            |b| b.write = vec!["/var/lib/updater/hooks/tierstore/work".to_owned()],
        ),
    ];
    for (what, first, second) in cases {
        let loaded = collide(first, second);
        assert_eq!(
            conflict(&loaded, "tierstore"),
            (what, "other".to_owned()),
            "{what}"
        );
        assert_eq!(
            conflict(&loaded, "other"),
            (what, "tierstore".to_owned()),
            "{what}"
        );
        assert_eq!(
            loaded.services.keys().collect::<Vec<_>>(),
            ["tsb"],
            "{what}"
        );
    }
}

/// Catches two services claiming one unit: `tierstore` may name `tierstore-b.service` because it
/// begins with its name, and so may the service `tierstore-b`.
#[test]
fn two_declarations_that_claim_one_unit_are_both_refused() {
    let host = Host::new("unit-clash");
    let mut a = Spec::new("tierstore");
    a.units = vec!["tierstore-b.service".to_owned()];
    let mut b = Spec::new("tierstore-b");
    b.uid = "tsb".to_owned();
    b.hook_user = "tsb-update".to_owned();
    host.put(&a);
    host.put(&b);
    let loaded = host.load();
    assert_eq!(
        conflict(&loaded, "tierstore"),
        ("a unit", "tierstore-b".to_owned())
    );
    assert_eq!(
        conflict(&loaded, "tierstore-b"),
        ("a unit", "tierstore".to_owned())
    );
    assert!(loaded.services.is_empty());
}

/// Catches the list of refusals being in the order the directory happened to list: sorted by
/// file, so an alert reads the same on every host.
#[test]
fn refusals_are_in_file_order() {
    let host = Host::new("order");
    for name in ["zeta", "alpha", "mid"] {
        host.scratch_file(&format!("{name}.toml"), "name = [");
    }
    let loaded = host.load();
    let order: Vec<&str> = loaded.refused.iter().map(|r| r.service.as_str()).collect();
    assert_eq!(order, ["alpha", "mid", "zeta"]);
}

/// Catches the shared-account rule applying to root: several services run as root (the js-agent
/// and kbf among them), so two declarations whose service account is root both load. Their units,
/// ports and record directories still may not collide.
#[test]
fn two_declarations_of_services_that_run_as_root_both_load() {
    let host = Host::new("two-roots");
    for name in ["tierstore", "other"] {
        let mut spec = Spec::new(name);
        spec.uid = "root".to_owned();
        host.put(&spec);
    }
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
    assert_eq!(loaded.services.len(), 2);

    // The rules that are not about the account still hold between them.
    let host = Host::new("two-roots-clash");
    for name in ["tierstore", "other"] {
        let mut spec = Spec::new(name);
        spec.uid = "root".to_owned();
        spec.ports = vec![9301];
        host.put(&spec);
    }
    let loaded = host.load();
    assert_eq!(
        conflict(&loaded, "tierstore"),
        ("a health port", "other".to_owned())
    );
    assert_eq!(
        conflict(&loaded, "other"),
        ("a health port", "tierstore".to_owned())
    );
}

/// Catches a link re-pointed after the load: the targets were resolved once, when the file was
/// read, and a step that runs later must check again. Each kind of entry is re-pointed at a
/// reserved place and refused under its own field.
#[test]
fn a_target_pointed_at_a_reserved_place_after_the_load_is_refused_by_the_recheck() {
    let host = Host::new("recheck");
    let real = host.scratch.dir("real", 0o755);
    let (read, state) = (
        host.scratch.path().join("read-link"),
        host.scratch.path().join("state-link"),
    );
    for link in [&read, &state] {
        symlink(&real, link).unwrap();
    }
    let mut spec = Spec::new("tierstore");
    spec.read = vec![read.display().to_string()];
    spec.state_dir = state.display().to_string();
    host.put(&spec);
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
    let declaration = &loaded.services["tierstore"].declaration;
    assert_eq!(recheck_targets(declaration, 65534), Ok(()));
    for (link, field) in [(&read, "hook.read_paths"), (&state, "state_dir")] {
        fs::remove_file(link).unwrap();
        symlink("/etc", link).unwrap();
        match recheck_targets(declaration, 65534) {
            Err(Refusal::Reserved { field: found, .. }) => assert_eq!(found, field),
            other => panic!("{field}: {other:?}"),
        }
        fs::remove_file(link).unwrap();
        symlink(&real, link).unwrap();
        assert_eq!(
            recheck_targets(declaration, 65534),
            Ok(()),
            "{field} put back"
        );
    }
}

/// Catches a hook account that is another service's own account, which would let that hook
/// write what the other service owns and run as it; and two services sharing one hook account,
/// which would let each write the other's record. Root is exempt in both.
#[test]
fn a_hook_account_may_not_be_another_services_account_or_shared() {
    let host = Host::new("hook-accounts");
    let mut a = Spec::new("tierstore");
    a.hook_user = "other".to_owned();
    host.put(&a);
    host.put(&Spec::new("other"));
    host.put(&Spec::new("tsb"));
    let loaded = host.load();
    assert_eq!(
        conflict(&loaded, "tierstore"),
        (
            "a hook account that is another service's account",
            "other".to_owned()
        )
    );
    assert_eq!(
        conflict(&loaded, "other"),
        (
            "a hook account that is another service's account",
            "tierstore".to_owned()
        )
    );
    assert_eq!(loaded.services.keys().collect::<Vec<_>>(), ["tsb"]);

    let host = Host::new("hook-shared");
    for name in ["tierstore", "other"] {
        let mut spec = Spec::new(name);
        spec.hook_user = "tsb-update".to_owned();
        host.put(&spec);
    }
    let loaded = host.load();
    assert_eq!(
        conflict(&loaded, "tierstore"),
        ("a hook account", "other".to_owned())
    );
    assert_eq!(
        conflict(&loaded, "other"),
        ("a hook account", "tierstore".to_owned())
    );

    // Root hooks are the one exception: several hooks run as root together.
    let host = Host::new("hook-roots");
    for name in ["tierstore", "other"] {
        let mut spec = Spec::new(name);
        spec.hook_user = "root".to_owned();
        host.put(&spec);
    }
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
}

/// Catches the race a root hook is open to: `ReadWritePaths=` follows links, so a path whose
/// parent a non-root account can write can be swapped for a link to `/etc` after the check and
/// before the unit starts. For a hook that runs as root every directory above a read or write
/// path must be root's and closed to the group and others. Here the test's own account owns
/// the directories above a path in the scratch directory, and `/tmp` is open to everyone.
#[test]
fn a_root_hook_path_below_a_directory_a_non_root_account_controls_is_refused() {
    let host = Host::new("ancestors");
    let owned = host.scratch.path().join("data").display().to_string();
    for (path, ancestor) in [
        (owned.as_str(), host.scratch.path().display().to_string()),
        ("/tmp/updater-ancestor-probe", "/tmp".to_owned()),
    ] {
        let mut spec = Spec::new("tierstore");
        spec.hook_user = "root".to_owned();
        spec.read = vec![path.to_owned()];
        host.put(&spec);
        let loaded = host.load();
        match refused(&loaded, "tierstore") {
            Refusal::Ancestor {
                field,
                path: found,
                ancestor: above,
            } => {
                assert_eq!(
                    (field, found.as_str(), above),
                    ("hook.read_paths", path, ancestor)
                );
            }
            other => panic!("{path}: {other:?}"),
        }
    }
}

/// Catches the rule being applied where it does not belong or refusing what is safe: a hook that
/// is not root is not held to it, and a root hook with paths below root's closed directories
/// loads (a read path in `/usr/sbin`, a write path below `/opt` that does not exist yet).
#[test]
fn the_ancestor_rule_is_for_root_hooks_and_passes_root_owned_directories() {
    let host = Host::new("ancestors-ok");
    let mut elsewhere = Spec::new("tierstore");
    elsewhere.read = vec![host.scratch.path().join("data").display().to_string()];
    host.put(&elsewhere);
    assert!(host.load().refused.is_empty());

    let mut root = Spec::new("tierstore");
    root.hook_user = "root".to_owned();
    root.read = vec!["/usr/sbin/nft".to_owned()];
    root.write = vec!["/var/lib/tierstore-probe/work".to_owned()];
    host.put(&root);
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
}

/// Catches the root checks keying on the name `root`: an account with uid 0 under another name is
/// root, and a hook that runs as it is held to the ancestor rule and takes no string parameter.
#[test]
fn a_hook_account_with_uid_zero_under_another_name_is_a_root_hook() {
    let host = Host::new("toor");
    let mut spec = Spec::new("tierstore");
    spec.hook_user = "toor".to_owned();
    spec.read = vec![host.scratch.path().join("data").display().to_string()];
    host.put(&spec);
    let loaded = host.load();
    assert!(
        matches!(refused(&loaded, "tierstore"), Refusal::Ancestor { .. }),
        "{:?}",
        loaded.refused
    );

    let mut plain = Spec::new("tierstore");
    plain.hook_user = "toor".to_owned();
    host.scratch_file(
        "tierstore.toml",
        &format!("{}\n[hook.params]\nname = \"a string\"\n", plain.text()),
    );
    let loaded = host.load();
    match refused(&loaded, "tierstore") {
        Refusal::Value { field, .. } => assert_eq!(field, "hook.params"),
        other => panic!("{other:?}"),
    }
}

/// Catches `recheck_targets` ignoring whether the hook is root: told it is, it applies the
/// ancestor rule to the same declaration it passes when told it is not.
#[test]
fn the_recheck_applies_the_ancestor_rule_when_told_the_hook_is_root() {
    let host = Host::new("recheck-root");
    let mut spec = Spec::new("tierstore");
    spec.read = vec![host.scratch.path().join("data").display().to_string()];
    host.put(&spec);
    let loaded = host.load();
    let declaration = &loaded.services["tierstore"].declaration;
    assert_eq!(recheck_targets(declaration, 65534), Ok(()));
    assert!(matches!(
        recheck_targets(declaration, 0),
        Err(Refusal::Ancestor { .. })
    ));
}

/// Catches a root hook being granted a directory of the system that happens to be one component
/// under a write root (`/var/lib/sudo`, `/run/udev`, `/var/lib/cloud`): it may write at or below a
/// directory named for its service, and a hook that is not root keeps the one-component rule.
#[test]
fn a_root_hook_may_write_only_below_a_directory_named_for_its_service() {
    let host = Host::new("service-dirs");
    for account in ["root", "toor"] {
        for path in [
            "/var/lib/chrony",
            "/run/chrony",
            "/var/lib/other",
            "/var/lib/tierstorex",
            "/var/cache/other/tierstore",
        ] {
            let mut spec = Spec::new("tierstore");
            spec.hook_user = account.to_owned();
            spec.write = vec![path.to_owned()];
            host.put(&spec);
            let loaded = host.load();
            match refused(&loaded, "tierstore") {
                Refusal::Reserved { field, why, .. } => {
                    assert_eq!(
                        (field, why),
                        ("hook.write_paths", Violation::NotTheServices),
                        "{account} {path}"
                    );
                }
                other => panic!("{account} {path}: {other:?}"),
            }
        }
        let mut spec = Spec::new("tierstore");
        spec.hook_user = account.to_owned();
        spec.write = [
            "/var/lib/tierstore",
            "/var/lib/tierstore/a/b",
            "/var/cache/tierstore-state",
            "/run/tierstore",
        ]
        .map(str::to_owned)
        .to_vec();
        host.put(&spec);
        let loaded = host.load();
        assert!(loaded.refused.is_empty(), "{account}: {:?}", loaded.refused);
    }
    let mut ordinary = Spec::new("tierstore");
    ordinary.write = vec!["/var/lib/chrony".to_owned()];
    host.put(&ordinary);
    assert!(host.load().refused.is_empty());
}

/// A spawner that succeeds at once, for the tests that only ask whether a step is started.
struct Silent;

impl Spawner for Silent {
    fn run(&self, _command: &SystemdRun) -> std::io::Result<Finished> {
        Ok(Finished {
            code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

/// Catches the root rules being a matter of how a `Launch` or a recheck is called: for a loaded
/// declaration the hook's uid is what the loader resolved, and a uid of 0 gets the ancestor rule
/// from `Declared::recheck` and from a `Launch` made of it, with nothing a caller can pass to
/// turn it off. The declaration here names an ordinary account, so only the uid can decide.
#[test]
fn a_loaded_declaration_whose_hook_has_uid_zero_is_held_to_the_root_rules_however_it_is_called() {
    let host = Host::new("uid-zero");
    let mut spec = Spec::new("tierstore");
    host.put(&spec);
    spec.read = vec![host.scratch.path().join("data").display().to_string()];
    let mut declared = {
        let loaded = host.load();
        loaded.services["tierstore"].clone()
    };
    declared.declaration.hook.read_paths = spec.read.iter().cloned().collect();
    let release = host.scratch.path();

    declared.hook = UserIds {
        uid: 65534,
        gid: 65534,
    };
    assert_eq!(declared.recheck(), Ok(()));
    let ordinary = Launch::of(&declared, Step::Verify, 1, release, None);
    assert!(!ordinary.root_hook());
    run_step(&Silent, &ordinary).expect("an ordinary hook");

    declared.hook = UserIds { uid: 0, gid: 0 };
    assert!(matches!(declared.recheck(), Err(Refusal::Ancestor { .. })));
    let root = Launch::of(&declared, Step::Verify, 1, release, None);
    assert!(root.root_hook());
    assert!(matches!(
        run_step(&Silent, &root),
        Err(update_rights::RunError::Recheck(Refusal::Ancestor { .. }))
    ));
    let by_hand = Launch::new(&declared.declaration, 0, Step::Verify, 1, release, None);
    assert!(matches!(
        run_step(&Silent, &by_hand),
        Err(update_rights::RunError::Recheck(Refusal::Ancestor { .. }))
    ));
}

/// Catches a root hook writing in another declared service's directory through the `-` rule:
/// the service `tierstore` may write `/var/lib/tierstore-state`, which begins with its name and a
/// dash, but not `/var/lib/tierstore-b/x`, where `tierstore-b` is a service of this host. The
/// directory belongs to the longest declared name it begins with, and a service's own stays its
/// own. The rule is across declarations, like the conflicts, and is for hooks that run as root.
#[test]
fn a_root_hook_may_not_write_in_the_directory_of_another_declared_service() {
    let host = Host::new("other-services");
    let mut a = Spec::new("tierstore");
    a.hook_user = "root".to_owned();
    a.write = vec![
        "/var/lib/tierstore-state".to_owned(),
        "/var/lib/tierstore-b/x".to_owned(),
    ];
    let mut b = Spec::new("tierstore-b");
    b.uid = "tsb".to_owned();
    b.hook_user = "root".to_owned();
    b.write = vec![
        "/var/lib/tierstore-b".to_owned(),
        "/var/lib/tierstore-b-state/y".to_owned(),
    ];
    host.put(&a);
    host.put(&b);
    let loaded = host.load();
    assert_eq!(
        conflict(&loaded, "tierstore"),
        (
            "a write path in the directory of another service",
            "tierstore-b".to_owned()
        )
    );
    assert_eq!(loaded.services.keys().collect::<Vec<_>>(), ["tierstore-b"]);

    // Without the path into the other's directory both load, and a hook that is not root is not
    // held to the rule (it is held to the allow-list alone).
    a.write = vec!["/var/lib/tierstore-state".to_owned()];
    host.put(&a);
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
    a.write = vec!["/var/lib/tierstore-b/x".to_owned()];
    a.hook_user = "tierstore-update".to_owned();
    host.put(&a);
    let loaded = host.load();
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
}

/// Catches a service named for a directory of the system, which the rule that a root hook writes
/// in a directory named for its service would then hand to it: such a name is refused whatever
/// the hook runs as.
#[test]
fn a_service_may_not_be_named_for_a_directory_of_the_system() {
    let host = Host::new("system-names");
    for name in [
        "sudo", "udev", "systemd", "polkit-1", "apt", "dpkg", "cron", "network",
    ] {
        let mut spec = Spec::new(name);
        spec.uid = "tierstore".to_owned();
        spec.hook_user = "root".to_owned();
        host.put(&spec);
        let loaded = host.load();
        match refused(&loaded, name) {
            Refusal::Value { field, .. } => assert_eq!(field, "name", "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        fs::remove_file(host.dir.join(format!("{name}.toml"))).unwrap();
    }
}
