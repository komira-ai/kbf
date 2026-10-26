//! The profile's VM user keys: `vm_user_uid` and `autologin=vm`.

mod support;

use kbf_mac_provision::profile::Profile;
use support::FakeMac;

/// The test profile less its pool lines and `autologin`/`filevault` lines, with `lines`
/// appended (`edit` may replace those two lines first).
fn edited(edit: impl FnOnce(&mut Vec<String>)) -> String {
    let mac = FakeMac::fresh("profile-vm-edit");
    let mut lines: Vec<String> = mac
        .profile_text()
        .lines()
        .filter(|l| !l.starts_with("pool_users=") && !l.starts_with("pool_first_uid="))
        .map(str::to_owned)
        .collect();
    edit(&mut lines);
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn set(key: &str, value: &str) -> impl FnOnce(&mut Vec<String>) {
    let (key, value) = (key.to_owned(), value.to_owned());
    move |lines| {
        let line = lines
            .iter_mut()
            .find(|l| l.starts_with(&format!("{key}=")))
            .unwrap();
        *line = format!("{key}={value}");
    }
}

fn append(line: &str) -> impl FnOnce(&mut Vec<String>) {
    let line = line.to_owned();
    move |lines| lines.push(line)
}

fn refusal(text: &str) -> String {
    match Profile::parse(text.as_bytes()) {
        Ok(_) => "accepted".into(),
        Err(e) => e.to_string(),
    }
}

/// Catches: a `vm_user_uid` outside 450 to 499 accepted (below is a system id, at 500
/// or above is a visible account), one that is not a number, or one that is the role
/// account's id; and the key set to nothing refused (an empty optional key is unset).
#[test]
fn a_vm_user_uid_is_a_number_from_450_to_499_and_not_the_role_id() {
    for (value, said) in [
        ("449", "vm_user_uid: 449 is not a number from 450 to 499"),
        ("500", "vm_user_uid: 500 is not a number from 450 to 499"),
        ("46x", "vm_user_uid: 46x is not a number from 450 to 499"),
        ("-460", "vm_user_uid: -460 is not a number from 450 to 499"),
        ("480", "vm_user_uid: 480 is role_id"),
    ] {
        assert_eq!(
            refusal(&edited(append(&format!("vm_user_uid={value}")))),
            said
        );
    }
    for value in ["450", "460", "499", ""] {
        assert_eq!(
            refusal(&edited(append(&format!("vm_user_uid={value}")))),
            "accepted",
            "{value:?}"
        );
    }
    let p = Profile::parse(edited(append("vm_user_uid=460")).as_bytes()).unwrap();
    assert_eq!(p.vm_user_uid, Some(460));
    assert!(p.print().contains("vm_user_uid=460\n"));
    let none = Profile::parse(edited(|_| {}).as_bytes()).unwrap();
    assert_eq!(none.vm_user_uid, None);
    assert!(!none.print().contains("vm_user_uid"));
}

/// Catches: a profile that wants a VM user and says FileVault is on accepted (the
/// Mac would stop at FileVault's unlock screen after every boot), and `autologin=vm`
/// accepted without a VM user to log in as.
#[test]
fn a_vm_user_needs_filevault_off_and_autologin_vm_needs_a_vm_user() {
    assert_eq!(
        refusal(&edited(|l| {
            set("filevault", "on")(l);
            l.push("vm_user_uid=460".into());
        })),
        "vm_user_uid: filevault=on: the VM user logs in automatically, which FileVault forbids"
    );
    assert_eq!(
        refusal(&edited(set("autologin", "vm"))),
        "autologin: vm needs vm_user_uid"
    );
    assert_eq!(
        refusal(&edited(|l| {
            set("autologin", "vm")(l);
            l.push("vm_user_uid=460".into());
        })),
        "accepted"
    );
    // FileVault on is fine for a profile with no VM user.
    assert_eq!(refusal(&edited(set("filevault", "on"))), "accepted");
}
