//! The accounts: the one administrator, the daemon's role account, and auto-login.

use std::io;

use crate::items::{Ctx, DEFAULTS, Lines, Outcome, defaults_read, fail, or, pass, run};

const DSCL: &str = "/usr/bin/dscl";
const DSEDITGROUP: &str = "/usr/sbin/dseditgroup";
const LOGINWINDOW: &str = "/Library/Preferences/com.apple.loginwindow";
const KCPASSWORD: &str = "/etc/kcpassword";
/// kbf-mac-session's intent record: the lease whose user the session switch logs in.
const SESSION_INTENT: &str = "/var/db/kbf-mac-session/session-intent";

/// The first word of attribute `attr` of directory record `record`, or empty. dscl
/// prints an attribute outside its standard set with a prefix:
/// `dsAttrTypeNative:IsHidden: 1`.
fn dscl_attr(c: &Ctx<'_>, record: &str, attr: &str) -> String {
    let plain = format!("{attr}:");
    let native = format!("dsAttrTypeNative:{attr}:");
    c.mac
        .out(DSCL, &[".", "-read", record, attr])
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next().unwrap_or_default();
            (name == plain || name == native).then(|| words.next().unwrap_or_default().to_owned())
        })
        .unwrap_or_default()
}

/// Whether `dseditgroup` counts `user` as a member of `admin`: listed, by primary group
/// 80, or through a nested group.
fn is_admin(c: &Ctx<'_>, user: &str) -> bool {
    c.mac
        .succeeds(DSEDITGROUP, &["-o", "checkmember", "-m", user, "admin"])
}

/// `admin_user` exists, is an administrator, and it and `root` are the only ones: the
/// names in `admin`'s `GroupMembership`, and every account `dseditgroup` counts.
pub(crate) fn admin_user(c: &Ctx<'_>) -> Outcome {
    let admin = c.value();
    let listed = c
        .mac
        .out(DSCL, &[".", "-read", "/Groups/admin", "GroupMembership"]);
    let mut names: Vec<String> = listed
        .lines()
        .flat_map(|line| {
            line.strip_prefix("GroupMembership:")
                .unwrap_or(line)
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    let users = c.mac.out(DSCL, &[".", "-list", "/Users"]);
    names.extend(
        users
            .split_whitespace()
            .filter(|u| is_admin(c, u))
            .map(str::to_owned),
    );
    let mut others: Vec<&str> = Vec::new();
    for name in &names {
        if name != "root" && name != admin && !others.contains(&name.as_str()) {
            others.push(name);
        }
    }
    if dscl_attr(c, &format!("/Users/{admin}"), "UniqueID").is_empty() {
        fail(format!("no account {admin}"))
    } else if !is_admin(c, admin) {
        fail(format!("{admin} is not in the admin group"))
    } else if !others.is_empty() {
        fail(format!("other administrators: {}", others.join(" ")))
    } else {
        pass(format!("{admin} is the only administrator"))
    }
}

/// Another user or group that holds the role id: `uid|gid ID belongs to NAME`.
fn role_id_taken(c: &Ctx<'_>) -> Option<String> {
    let me = c.value();
    let id = u64::from(c.profile.role_id);
    [
        ("uid", "/Users", "UniqueID"),
        ("gid", "/Groups", "PrimaryGroupID"),
    ]
    .into_iter()
    .find_map(|(kind, dir, attr)| {
        c.mac
            .out(DSCL, &[".", "-list", dir, attr])
            .lines()
            .find_map(|line| {
                let mut words = line.split_whitespace();
                let name = words.next().unwrap_or_default();
                let value = words.next().unwrap_or_default();
                (value.parse::<u64>() == Ok(id) && name != me)
                    .then(|| format!("{kind} {id} belongs to {name}"))
            })
    })
}

/// The role account and its group: the role id, no shell, home `/var/empty`, hidden,
/// and no password (`Password` is `*` and no `AuthenticationAuthority` names a hash);
/// no other user or group holds the id.
pub(crate) fn role_user(c: &Ctx<'_>) -> Outcome {
    let role = c.value();
    let id = c.profile.role_id;
    let record = format!("/Users/{role}");
    let want = format!(
        "UniqueID={id} PrimaryGroupID={id} UserShell=/usr/bin/false NFSHomeDirectory=/var/empty IsHidden=1 Password=* AuthenticationAuthority=absent group={id}"
    );
    let mut have: Vec<String> = [
        "UniqueID",
        "PrimaryGroupID",
        "UserShell",
        "NFSHomeDirectory",
        "IsHidden",
        "Password",
    ]
    .iter()
    .map(|a| format!("{a}={}", dscl_attr(c, &record, a)))
    .collect();
    // dscl may print this value on a line of its own: test the attribute's name.
    let authority = c
        .mac
        .out(DSCL, &[".", "-read", &record, "AuthenticationAuthority"])
        .lines()
        .any(|l| l.starts_with("AuthenticationAuthority:"));
    have.push(format!(
        "AuthenticationAuthority={}",
        if authority { "present" } else { "absent" }
    ));
    have.push(format!(
        "group={}",
        dscl_attr(c, &format!("/Groups/{role}"), "PrimaryGroupID")
    ));
    let have = have.join(" ");
    if let Some(taken) = role_id_taken(c) {
        fail(taken)
    } else if have == want {
        pass(format!(
            "{role} (uid and gid {id}, hidden, no shell, no password)"
        ))
    } else {
        fail(format!("want {want}, found {have}"))
    }
}

/// `0` or `[1-9][0-9]*`.
fn lease_number(s: &str) -> bool {
    s == "0" || (!s.starts_with('0') && !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

/// The lease user kbf-mac-session's intent record names: `kbf-lease-<term>-<seq>` for
/// a record that is one line `<term>.<seq>` and not a link.
fn intent_user(c: &Ctx<'_>) -> Option<String> {
    if c.mac.lstat(SESSION_INTENT)?.kind == 'l' {
        return None;
    }
    let record = String::from_utf8(c.mac.read(SESSION_INTENT)?).ok()?;
    let (term, seq) = record.strip_suffix('\n')?.split_once('.')?;
    (lease_number(term) && lease_number(seq)).then(|| format!("kbf-lease-{term}-{seq}"))
}

/// Auto-login is off with no `/etc/kcpassword` left, or it is the lease user whose
/// lease kbf-mac-session's intent record names, with a uid in `lease_uids`.
pub(crate) fn autologin(c: &Ctx<'_>) -> Outcome {
    let user = defaults_read(c, LOGINWINDOW, "autoLoginUser");
    if user.is_empty() {
        return if c.mac.exists(KCPASSWORD) {
            fail("off, but /etc/kcpassword is left over")
        } else {
            pass("off")
        };
    }
    let uid = dscl_attr(c, &format!("/Users/{user}"), "UniqueID");
    let in_range = crate::pattern::number(&uid)
        && uid
            .parse::<u64>()
            .is_ok_and(|u| c.profile.lease_uids.contains(&u));
    if user.starts_with("kbf-lease-") && in_range {
        return if intent_user(c).as_deref() == Some(user.as_str()) {
            pass(format!(
                "lease user {user} (uid {uid}), set by kbf-mac-session for a whole_machine lease"
            ))
        } else {
            fail(format!(
                "auto-login is {user} (uid {uid}), but no kbf-mac-session intent record names its lease: a lease user left behind; apply clears auto-login and /etc/kcpassword"
            ))
        };
    }
    fail(format!(
        "auto-login is {user} (uid {}); only a kbf-lease-* user in uids {} may be",
        or(&uid, "none"),
        c.profile.get("lease_uids")
    ))
}

/// Makes the role account and its group, unless another user or group holds the role
/// id: the id, no shell, home `/var/empty`, hidden, `Password` `*` and no
/// `AuthenticationAuthority`.
pub(crate) fn fix_role_user(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    if role_id_taken(c).is_some() {
        return Ok(());
    }
    let role = c.value();
    let id = c.profile.role_id.to_string();
    let group = format!("/Groups/{role}");
    let user = format!("/Users/{role}");
    run(c, DSCL, &[".", "-create", &group]);
    run(c, DSCL, &[".", "-create", &group, "PrimaryGroupID", &id]);
    run(c, DSCL, &[".", "-create", &group, "Password", "*"]);
    run(c, DSCL, &[".", "-create", &user]);
    for (attr, value) in [
        ("UniqueID", id.as_str()),
        ("PrimaryGroupID", id.as_str()),
        ("UserShell", "/usr/bin/false"),
        ("NFSHomeDirectory", "/var/empty"),
        ("IsHidden", "1"),
        ("Password", "*"),
    ] {
        run(c, DSCL, &[".", "-create", &user, attr, value]);
    }
    run(c, DSCL, &[".", "-delete", &user, "AuthenticationAuthority"]);
    out.change(c.key, format!("role account and group {role}, id {id}"));
    Ok(())
}

/// Clears auto-login and removes `/etc/kcpassword`, whichever is there.
pub(crate) fn fix_autologin(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    if !defaults_read(c, LOGINWINDOW, "autoLoginUser").is_empty() {
        let domain = c.mac.path(LOGINWINDOW);
        run(c, DEFAULTS, &["delete", &domain, "autoLoginUser"]);
        out.change(c.key, "auto-login cleared");
    }
    if c.mac.lstat(KCPASSWORD).is_some() {
        c.mac.remove(KCPASSWORD)?;
        out.change(c.key, "removed /etc/kcpassword");
    }
    Ok(())
}
