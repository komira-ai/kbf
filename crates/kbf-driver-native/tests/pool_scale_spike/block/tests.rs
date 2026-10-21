//! The spike's creation and removal rules against a fake Mac, on every platform: what it
//! removes, what it refuses to touch, and that a panic cannot leave users behind.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use super::*;

type Attrs = BTreeMap<String, String>;

#[derive(Default)]
struct State {
    users: BTreeMap<String, (Option<u32>, Attrs)>,
    groups: BTreeMap<String, Option<u32>>,
    homes: BTreeMap<String, Home>,
    homes_dir: bool,
    folders: BTreeMap<String, u32>,
    /// Uids with a process the fake cannot end.
    stuck: BTreeSet<u32>,
    ended: Vec<u32>,
    /// Everything `delete`, `remove_home`, `remove_folder` was asked to remove.
    removed: Vec<String>,
}

#[derive(Default)]
struct Fake(RefCell<State>);

impl Fake {
    /// A record, group, home or folder the spike did not make.
    fn foreign_user(&self, name: &str, uid: Option<u32>, attrs: &[(&str, &str)]) {
        let attrs = attrs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.0
            .borrow_mut()
            .users
            .insert(name.to_owned(), (uid, attrs));
    }

    fn removed(&self) -> Vec<String> {
        self.0.borrow().removed.clone()
    }
}

impl Mac for Fake {
    fn users(&self) -> Result<Vec<(String, Option<u32>)>, String> {
        Ok(self
            .0
            .borrow()
            .users
            .iter()
            .map(|(n, (u, _))| (n.clone(), *u))
            .collect())
    }

    fn groups(&self) -> Result<Vec<(String, Option<u32>)>, String> {
        Ok(self
            .0
            .borrow()
            .groups
            .iter()
            .map(|(n, g)| (n.clone(), *g))
            .collect())
    }

    fn attrs(&self, record: &str, wanted: &[&str]) -> Result<Vec<(String, String)>, String> {
        let name = record.strip_prefix("/Users/").ok_or("not a user")?;
        let state = self.0.borrow();
        let (_, attrs) = state.users.get(name).ok_or("no such record")?;
        Ok(wanted
            .iter()
            .filter_map(|w| Some(((*w).to_owned(), attrs.get(*w)?.clone())))
            .collect())
    }

    fn create(&self, record: &str, attr: Option<(&str, &str)>) -> Result<(), String> {
        let mut state = self.0.borrow_mut();
        if let Some(name) = record.strip_prefix("/Users/") {
            let entry = state.users.entry(name.to_owned()).or_default();
            if let Some((a, v)) = attr {
                if a == "UniqueID" {
                    entry.0 = Some(v.parse().map_err(|_| "uid")?);
                }
                entry.1.insert(a.to_owned(), v.to_owned());
            }
        } else if let Some(name) = record.strip_prefix("/Groups/") {
            let entry = state.groups.entry(name.to_owned()).or_default();
            if let Some(("PrimaryGroupID", v)) = attr {
                *entry = Some(v.parse().map_err(|_| "gid")?);
            }
        }
        Ok(())
    }

    fn delete(&self, record: &str) -> Result<(), String> {
        let mut state = self.0.borrow_mut();
        state.removed.push(record.to_owned());
        let gone = match record.split_once('/').and_then(|(_, r)| r.split_once('/')) {
            Some(("Users", n)) => state.users.remove(n).is_some(),
            Some(("Groups", n)) => state.groups.remove(n).is_some(),
            _ => false,
        };
        gone.then_some(())
            .ok_or_else(|| "no such record".to_owned())
    }

    fn homes(&self) -> Result<Vec<Home>, String> {
        Ok(self.0.borrow().homes.values().cloned().collect())
    }

    fn make_homes_dir(&self) -> Result<(), String> {
        self.0.borrow_mut().homes_dir = true;
        Ok(())
    }

    fn make_home(&self, name: &str, uid: u32, _gid: u32) -> Result<(), String> {
        self.0.borrow_mut().homes.insert(
            name.to_owned(),
            Home {
                name: name.to_owned(),
                uid,
                is_dir: true,
                empty: true,
            },
        );
        Ok(())
    }

    fn remove_home(&self, name: &str) -> Result<(), String> {
        let mut state = self.0.borrow_mut();
        state.removed.push(format!("home {name}"));
        state.homes.remove(name);
        Ok(())
    }

    fn remove_homes_dir(&self) {
        let mut state = self.0.borrow_mut();
        if state.homes.is_empty() {
            state.homes_dir = false;
        }
    }

    fn homes_dir_left(&self) -> bool {
        self.0.borrow().homes_dir
    }

    fn folders(&self) -> Result<Vec<(String, u32)>, String> {
        Ok(self
            .0
            .borrow()
            .folders
            .iter()
            .map(|(p, u)| (p.clone(), *u))
            .collect())
    }

    fn remove_folder(&self, path: &str) -> Result<(), String> {
        let mut state = self.0.borrow_mut();
        state.removed.push(format!("folder {path}"));
        state.folders.remove(path);
        Ok(())
    }

    fn end(&self, uid: u32) -> Result<(), String> {
        let mut state = self.0.borrow_mut();
        state.ended.push(uid);
        if state.stuck.contains(&uid) {
            Err("a process survives".to_owned())
        } else {
            Ok(())
        }
    }

    fn flush(&self) {}
}

/// A fake with `n` users made as the spike makes them.
fn made(n: u32) -> Fake {
    let mac = Fake::default();
    prepare(&mac).expect("prepare");
    for i in 0..n {
        create_user(&mac, i).expect("create");
    }
    mac
}

#[test]
fn a_user_made_by_the_spike_is_the_spikes() {
    let mac = made(3);
    mac.0
        .borrow_mut()
        .folders
        .insert("/private/var/folders/zz/abc".into(), uid_of(1));
    let s = survey(&mac).expect("survey");
    assert_eq!(s.users, vec![0, 1, 2]);
    assert!(s.group);
    assert_eq!(s.homes, vec!["kbf-scale-0", "kbf-scale-1", "kbf-scale-2"]);
    assert_eq!(
        s.folders,
        vec![("/private/var/folders/zz/abc".to_owned(), uid_of(1))]
    );
    assert!(s.foreign.is_empty(), "{:?}", s.foreign);
}

#[test]
fn a_sweep_removes_every_user_home_folder_and_the_group_and_ends_their_processes_first() {
    let mac = made(4);
    mac.0
        .borrow_mut()
        .folders
        .insert("/private/var/folders/zz/abc".into(), uid_of(2));
    let swept = sweep(&mac).expect("sweep");
    assert_eq!(
        (swept.users, swept.homes, swept.folders, swept.group),
        (4, 4, 1, true)
    );
    assert!(swept.errors.is_empty() && swept.foreign.is_empty());
    assert_eq!(mac.0.borrow().ended, (0..4).map(uid_of).collect::<Vec<_>>());
    assert_eq!(leftovers(&mac), Vec::<String>::new());
}

#[test]
fn the_start_sweep_finishes_a_run_that_was_killed_half_way() {
    let mac = made(2);
    // Killed after the home and the record, before any attribute: no uid, no marker.
    mac.0.borrow_mut().homes.insert(
        "kbf-scale-7".into(),
        Home {
            name: "kbf-scale-7".into(),
            uid: 0,
            is_dir: true,
            empty: true,
        },
    );
    mac.foreign_user("kbf-scale-7", None, &[]);
    // Killed after the marker.
    mac.foreign_user("kbf-scale-8", None, &[("RealName", MARK)]);
    let swept = start(&mac).expect("start");
    assert_eq!(swept.users, 4);
    assert_eq!(leftovers(&mac), Vec::<String>::new());
}

#[test]
fn a_record_that_holds_a_uid_of_the_block_is_refused_and_untouched() {
    let mac = made(2);
    mac.foreign_user("someone", Some(uid_of(500)), &[("RealName", "A person")]);
    let err = start(&mac).expect_err("must refuse");
    assert!(err.contains("someone") && err.contains("refusing"), "{err}");
    assert!(mac.0.borrow().users.contains_key("someone"));
    assert!(
        !mac.removed().iter().any(|r| r.contains("someone")),
        "{:?}",
        mac.removed()
    );
    // What was the spike's was removed, so a person can look at the rest.
    assert_eq!(survey(&mac).expect("survey").users, Vec::<u32>::new());
}

#[test]
fn a_record_named_like_a_test_user_with_another_uid_is_not_removed() {
    let mac = Fake::default();
    mac.foreign_user("kbf-scale-3", Some(1234), &[]);
    assert!(start(&mac).is_err());
    assert!(mac.removed().is_empty(), "{:?}", mac.removed());
}

#[test]
fn a_record_with_the_right_name_and_uid_but_another_real_name_is_not_removed() {
    let mac = Fake::default();
    mac.foreign_user("kbf-scale-3", Some(uid_of(3)), &[("RealName", "Pool user")]);
    assert!(start(&mac).is_err());
    assert!(mac.removed().is_empty(), "{:?}", mac.removed());
}

#[test]
fn a_record_with_the_marker_but_another_home_is_not_removed() {
    let mac = Fake::default();
    let elsewhere = format!("/{}/{}", "Users", user_name(3));
    mac.foreign_user(
        "kbf-scale-3",
        Some(uid_of(3)),
        &[("RealName", MARK), ("NFSHomeDirectory", elsewhere.as_str())],
    );
    assert!(start(&mac).is_err());
    assert!(mac.removed().is_empty(), "{:?}", mac.removed());
}

#[test]
fn a_name_outside_the_block_or_not_canonical_is_nobody_s_to_remove() {
    for name in [
        "kbf-scale-1024",
        "kbf-scale-007",
        "kbf-scale-",
        "kbf-scale-x",
        "kbf-pool-3",
    ] {
        assert_eq!(index_of(name), None, "{name}");
        let mac = Fake::default();
        mac.foreign_user(name, Some(5000), &[("RealName", MARK)]);
        assert!(start(&mac).is_ok(), "{name} is unrelated to the block");
        assert!(mac.removed().is_empty(), "{name}: {:?}", mac.removed());
    }
    assert_eq!(index_of("kbf-scale-0"), Some(0));
    assert_eq!(index_of("kbf-scale-1023"), Some(1023));
}

#[test]
fn a_group_that_holds_the_gid_or_the_name_with_another_gid_is_refused_and_untouched() {
    let mac = Fake::default();
    mac.0
        .borrow_mut()
        .groups
        .insert("staff2".into(), Some(FIRST_UID + 5));
    assert!(start(&mac).expect_err("gid held").contains("staff2"));
    assert!(mac.removed().is_empty());

    let mac = Fake::default();
    mac.0.borrow_mut().groups.insert(GROUP.into(), Some(4242));
    assert!(start(&mac).is_err());
    assert!(mac.removed().is_empty(), "{:?}", mac.removed());
}

#[test]
fn a_home_of_another_uid_or_a_link_or_an_unknown_entry_is_not_removed() {
    let mac = made(1);
    {
        let mut s = mac.0.borrow_mut();
        let entry = |name: &str, uid, is_dir| Home {
            name: name.into(),
            uid,
            is_dir,
            empty: false,
        };
        s.homes
            .insert("kbf-scale-1".into(), entry("kbf-scale-1", 501, true));
        s.homes
            .insert("kbf-scale-2".into(), entry("kbf-scale-2", uid_of(2), false));
        s.homes.insert("notes".into(), entry("notes", 0, true));
        // Root's, but with something in it: not a home the spike half made.
        s.homes
            .insert("kbf-scale-3".into(), entry("kbf-scale-3", 0, true));
    }
    let err = start(&mac).expect_err("foreign homes");
    assert!(
        err.contains("notes") && err.contains("kbf-scale-1"),
        "{err}"
    );
    let removed = mac.removed();
    assert_eq!(
        removed.iter().filter(|r| r.starts_with("home ")).count(),
        1,
        "{removed:?}"
    );
    assert!(removed.contains(&"home kbf-scale-0".to_owned()));
}

#[test]
fn a_folder_of_a_uid_a_foreign_record_holds_is_not_removed() {
    let mac = made(1);
    mac.foreign_user("someone", Some(uid_of(900)), &[]);
    mac.0
        .borrow_mut()
        .folders
        .insert("/private/var/folders/zz/theirs".into(), uid_of(900));
    mac.0
        .borrow_mut()
        .folders
        .insert("/private/var/folders/zz/ours".into(), uid_of(0));
    assert!(start(&mac).is_err());
    let removed = mac.removed();
    assert!(
        removed.contains(&"folder /private/var/folders/zz/ours".to_owned()),
        "{removed:?}"
    );
    assert!(!removed.iter().any(|r| r.contains("theirs")), "{removed:?}");
}

#[test]
fn a_user_with_a_process_that_will_not_end_is_left_with_its_home_and_folders() {
    let mac = made(2);
    mac.0.borrow_mut().stuck.insert(uid_of(1));
    mac.0
        .borrow_mut()
        .folders
        .insert("/private/var/folders/zz/stuck".into(), uid_of(1));
    let swept = sweep(&mac).expect("sweep");
    assert_eq!(swept.users, 1);
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    let removed = mac.removed();
    assert!(
        !removed.contains(&format!("/Users/{}", user_name(1))),
        "{removed:?}"
    );
    assert!(
        !removed.contains(&"home kbf-scale-1".to_owned()),
        "{removed:?}"
    );
    assert!(!removed.iter().any(|r| r.contains("stuck")), "{removed:?}");
    assert!(leftovers(&mac).iter().any(|l| l.contains("kbf-scale-1")));
}

#[test]
fn the_guard_removes_everything_when_a_panic_unwinds_through_it() {
    let mac = Fake::default();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = Guard::new(&mac);
        prepare(&mac).expect("prepare");
        for i in 0..5 {
            create_user(&mac, i).expect("create");
        }
        assert_eq!(survey(&mac).expect("survey").users.len(), 5);
        panic!("the phase failed");
    }));
    assert!(caught.is_err());
    assert_eq!(leftovers(&mac), Vec::<String>::new());
    assert_eq!(mac.0.borrow().ended.len(), 5);
}

#[test]
fn a_finished_guard_has_swept_once_and_its_drop_does_nothing_more() {
    let mac = Fake::default();
    let guard = Guard::new(&mac);
    prepare(&mac).expect("prepare");
    create_user(&mac, 0).expect("create");
    let swept = guard.finish().expect("finish");
    assert_eq!(swept.users, 1);
    assert_eq!(
        mac.0.borrow().ended,
        vec![uid_of(0)],
        "ended once, not again by the drop"
    );
    assert_eq!(leftovers(&mac), Vec::<String>::new());
}

#[test]
fn a_user_is_made_home_first_then_the_marker_then_the_pool_attributes() {
    let mac = made(1);
    let s = mac.0.borrow();
    let (uid, attrs) = &s.users["kbf-scale-0"];
    assert_eq!(*uid, Some(FIRST_UID));
    let want = [
        ("RealName", MARK),
        ("UniqueID", "61000"),
        ("PrimaryGroupID", "61000"),
        ("UserShell", SHELL),
        (
            "NFSHomeDirectory",
            "/private/var/kbf-scale-homes.noindex/kbf-scale-0",
        ),
        ("IsHidden", "1"),
        ("Password", "*"),
    ];
    for (k, v) in want {
        assert_eq!(attrs.get(k).map(String::as_str), Some(v), "{k}");
    }
    assert_eq!(s.homes["kbf-scale-0"].uid, FIRST_UID);
    assert_eq!(s.groups[GROUP], Some(FIRST_UID));
}

#[test]
fn the_batch_script_is_the_same_commands_in_the_same_order_as_one_dscl_each() {
    let script = batch_script(&[3, 4]);
    let lines: Vec<&str> = script.lines().collect();
    assert_eq!(lines.len(), 2 * 8);
    let (three, four) = (user_name(3), user_name(4));
    let record = |name: &str| format!("create /{}/{name}", "Users");
    assert_eq!(lines[0], record(&three));
    assert_eq!(lines[1], format!("{} RealName {MARK}", record(&three)));
    assert_eq!(lines[2], format!("{} UniqueID 61003", record(&three)));
    assert_eq!(lines[7], format!("{} Password *", record(&three)));
    assert_eq!(lines[8], record(&four));
    // The same record one `create` at a time ends as the script describes.
    let mac = Fake::default();
    create_user(&mac, 3).expect("create");
    let made = mac.0.borrow();
    let (uid, attrs) = &made.users["kbf-scale-3"];
    assert_eq!(*uid, Some(61003));
    for (attr, value) in user_attrs(3) {
        assert_eq!(attrs.get(attr), Some(&value), "{attr}");
    }
}
