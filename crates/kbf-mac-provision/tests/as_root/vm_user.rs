//! The VM user of `vm_user_uid` made on this runner as root: a module of `as_root.rs`
//! (its header says what this test applies and asserts), in the same test binary, so
//! it takes the same lock and runs in the same `cargo xtask sudo-test` step.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use kbf_mac_provision::vm_user::SHELL;

use super::{SCRATCH, USERS, dscl_value, free_id, host, output, owner_mode_links, prov};

/// Where the VM user's password is kept.
const VM_PASSWORD_FILE: &str = "/var/db/kbf-mac-session/vm-user-password";
const VM_STATE_DIR: &str = "/var/db/kbf-mac-session";
/// The VM user's home and directory record.
const VM_HOME: &str = concat!("/Users", "/kbf-vm");

/// Removes the VM user, its home and its password file when dropped, whether the test
/// passed or not; the state directory too when the test made it.
struct VmCleanup {
    state_dir_existed: bool,
}

impl Drop for VmCleanup {
    fn drop(&mut self) {
        Command::new("/usr/bin/dscl")
            .args([".", "-delete", VM_HOME])
            .output()
            .expect("ran dscl");
        let _ = fs::remove_dir_all(VM_HOME);
        let _ = fs::remove_file(VM_PASSWORD_FILE);
        if !self.state_dir_existed {
            let _ = fs::remove_dir(VM_STATE_DIR);
        }
        let _ = fs::remove_file(format!("{SCRATCH}/profile-vm"));
    }
}

/// Catches: the VM user made in a form this macOS does not take (an attribute missing,
/// no password, a user in `admin`), a password that is not the file's (`dscl -authonly`
/// is the proof, and it is the only place the test puts a password in an argument list:
/// the runner is a machine nothing reuses), a password file other than root's, mode
/// 400, a home that is not the user's with mode 700, a second apply that is not
/// idempotent on a real directory, and a repair that makes a new password. Makes the
/// account at a free uid from 450 and removes it; sets no auto-login.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn the_vm_user_is_made_with_a_root_only_password_file_and_checked() {
    let (_serial, host) = host();
    let existing = output("/usr/bin/dscl", &[".", "-list", USERS]);
    assert!(
        !existing.lines().any(|u| u == "kbf-vm") && !Path::new(VM_PASSWORD_FILE).exists(),
        "this Mac has a VM user already: the test would remove it"
    );
    let uid = free_id(450);
    assert!(uid <= 499, "no free uid from 450 to 499");
    let _cleanup = VmCleanup {
        state_dir_existed: Path::new(VM_STATE_DIR).exists(),
    };
    let profile = format!("{SCRATCH}/profile-vm");
    let text = fs::read_to_string(&host.profile).expect("read the profile");
    fs::write(&profile, format!("{text}vm_user_uid={uid}\n")).expect("write the VM profile");
    let apply = || {
        prov(&[
            "apply",
            "--profile",
            profile.as_str(),
            "--keys",
            "vm_user_uid",
        ])
    };
    let check = || {
        prov(&[
            "check",
            "--profile",
            profile.as_str(),
            "--keys",
            "vm_user_uid",
        ])
    };

    let made = apply();
    assert_eq!(made.status, Some(0), "{}{}", made.stdout, made.stderr);
    for change in [
        "CHANGE vm_user_uid: user kbf-vm, uid ",
        "CHANGE vm_user_uid: password of kbf-vm set (not shown)",
        concat!("CHANGE vm_user_uid: home /Users", "/kbf-vm, mode 700"),
    ] {
        assert!(
            made.stdout.contains(change),
            "lacks {change}\n{}",
            made.stdout
        );
    }
    let record = VM_HOME;
    for (attr, want) in [
        ("UniqueID", uid.to_string()),
        ("PrimaryGroupID", "20".to_owned()),
        ("UserShell", SHELL.to_owned()),
        ("NFSHomeDirectory", record.to_owned()),
        ("IsHidden", "1".to_owned()),
    ] {
        assert_eq!(dscl_value(record, attr), want, "{attr}");
    }
    assert!(
        output(
            "/usr/bin/dscl",
            &[".", "-read", record, "AuthenticationAuthority"]
        )
        .contains("ShadowHash"),
        "no password hash"
    );
    let admin = Command::new("/usr/sbin/dseditgroup")
        .args(["-o", "checkmember", "-m", "kbf-vm", "admin"])
        .output()
        .expect("ran dseditgroup");
    assert!(!admin.status.success(), "kbf-vm is an administrator");

    let (file_uid, file_gid, mode, links) = owner_mode_links(VM_PASSWORD_FILE);
    assert_eq!((file_uid, file_gid, mode, links), (0, 0, 0o400, 1));
    let (dir_uid, dir_gid, dir_mode, _) = owner_mode_links(VM_STATE_DIR);
    assert_eq!((dir_uid, dir_gid, dir_mode), (0, 0, 0o700));
    let password = fs::read_to_string(VM_PASSWORD_FILE).expect("read the password file");
    assert_eq!(password.len(), 64, "a password of 64 hex digits");
    assert!(password.bytes().all(|b| b.is_ascii_hexdigit()));
    for shown in [&made.stdout, &made.stderr] {
        assert!(!shown.contains(&password), "the password was printed");
    }
    let (home_uid, home_gid, home_mode, _) = owner_mode_links(record);
    assert_eq!((home_uid, home_gid, home_mode), (uid, 20, 0o700));
    let authenticated = Command::new("/usr/bin/dscl")
        .args([".", "-authonly", "kbf-vm", password.as_str()])
        .output()
        .expect("ran dscl");
    assert!(
        authenticated.status.success(),
        "the account does not have the file's password: {}",
        String::from_utf8_lossy(&authenticated.stdout)
    );

    // What the product relies on: a `passwd` for a record that is not there, fed to
    // `dscl .` on standard input, is a failure by its exit status or by a `DS Error`
    // line on its output (the product treats either as one).
    let mut child = Command::new("/usr/bin/dscl")
        .arg(".")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("started dscl");
    child
        .stdin
        .take()
        .expect("dscl's stdin")
        .write_all(concat!("passwd /Users", "/kbf-no-such-user x\n").as_bytes())
        .expect("fed dscl");
    let refused = child.wait_with_output().expect("dscl ran");
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !refused.status.success() || shown.contains("DS Error"),
        "dscl took a passwd for no record without a sign: {shown}"
    );

    let checked = check();
    assert_eq!(checked.status, Some(0), "{}", checked.stdout);
    assert!(checked.stdout.contains("PASS vm_user_uid: kbf-vm (uid "));
    let again = apply();
    assert_eq!(
        (again.status, again.stdout.as_str()),
        (Some(0), ""),
        "{}",
        again.stderr
    );

    // A shell changed by hand is made again; the password is not.
    let o = Command::new("/usr/bin/dscl")
        .args([".", "-create", record, "UserShell", "/usr/bin/false"])
        .output()
        .expect("ran dscl");
    assert!(o.status.success());
    let drifted = check();
    assert_eq!(drifted.status, Some(1), "{}", drifted.stdout);
    assert!(
        drifted
            .stdout
            .contains(&format!("UserShell=/usr/bin/false, want {SHELL}"))
    );
    let repaired = apply();
    let changes: Vec<&str> = repaired
        .stdout
        .lines()
        .filter(|l| l.starts_with("CHANGE "))
        .collect();
    assert_eq!(
        changes,
        [format!("CHANGE vm_user_uid: user kbf-vm, uid {uid}")]
    );
    assert_eq!(
        fs::read_to_string(VM_PASSWORD_FILE).expect("read the password file"),
        password
    );
    assert_eq!(dscl_value(record, "UserShell"), SHELL);
}
