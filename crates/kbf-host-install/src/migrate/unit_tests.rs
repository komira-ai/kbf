//! Reading a hand-placed unit.

use super::*;

const R: &str = "/var/lib/kbf-installer/releases";

fn unit(service: &str) -> String {
    format!(
        "[Unit]\nDescription=x\n\n[Service]\n{service}\n[Install]\nWantedBy=multi-user.target\n"
    )
}

/// The flags, the account and the few carried settings are what the unit gives, read the way
/// systemd reads the file: continuation lines are joined, comments dropped, an empty
/// assignment empties a list, the last of a single value wins, and only `[Service]` counts.
#[test]
fn a_unit_is_read_the_way_systemd_reads_it() {
    let text = format!(
        "[Unit]\nUser=not-this-one\n\n[Service]\n# a comment\nUser=a\nUser=kbf\nGroup=kbf\n\
         ; another\nExecStart=/bin/false\nExecStart=\nExecStart={R}/current/kbf --mode=daemon \\\n   --server https://frontend.example:8981 \\\n --node-id st1\n\
         Environment=A=1\nEnvironment=\nEnvironment=B=2\nEnvironmentFile=-/etc/kbf/env\n\
         WorkingDirectory=/srv\nWorkingDirectory=/var/lib/kbf\nWorkingDirectory=\nWorkingDirectory=/var/lib/kbf\nSupplementaryGroups=a\nSupplementaryGroups=b\n\
         NoSuchThing=1\nthis line has no assignment\n[Install]\nUser=late\n"
    );
    let unit = parse(&text, R).unwrap();
    assert_eq!(unit.program, format!("{R}/current/kbf"));
    assert_eq!(
        unit.args,
        [
            "--mode=daemon",
            "--server",
            "https://frontend.example:8981",
            "--node-id",
            "st1"
        ]
    );
    assert_eq!(
        (unit.user.as_deref(), unit.group.as_deref()),
        (Some("kbf"), Some("kbf"))
    );
    let carried: Vec<(&str, &str)> = unit.carried.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert_eq!(
        carried,
        [
            ("Environment", "B=2"),
            ("EnvironmentFile", "-/etc/kbf/env"),
            ("WorkingDirectory", "/var/lib/kbf"),
            ("SupplementaryGroups", "a"),
            ("SupplementaryGroups", "b"),
        ]
    );
}

/// An account assigned and then emptied is none; a unit without an account has none.
#[test]
fn an_emptied_account_is_none() {
    let unit = parse(
        &unit(&format!(
            "User=kbf\nUser=\nGroup=\nExecStart={R}/abc/kbf --mode=daemon\n"
        )),
        R,
    )
    .unwrap();
    assert_eq!((unit.user, unit.group), (None, None));
    assert!(unit.carried.is_empty());
}

/// What would have to be guessed stops the migration, and the line says what.
#[test]
fn what_would_have_to_be_guessed_is_refused() {
    let cases = [
        ("", "0 ExecStart= lines"),
        (
            "ExecStart=/bin/true\nExecStart=/bin/false\n",
            "2 ExecStart= lines",
        ),
        (&format!("ExecStart=-{R}/current/kbf\n"), "prefix character"),
        (&format!("ExecStart=+{R}/current/kbf\n"), "prefix character"),
        (
            "ExecStart=/usr/bin/kbf --mode=daemon\n",
            "not the kbf of a release",
        ),
        (
            &format!("ExecStart={R}/../x/kbf\n"),
            "not the kbf of a release",
        ),
        (
            &format!("ExecStart={R}/current/kbfx\n"),
            "not the kbf of a release",
        ),
        (
            &format!("ExecStart={R}current/kbf\n"),
            "not the kbf of a release",
        ),
        (
            &format!("ExecStart={R}/current/kbf --x=\"a b\"\n"),
            "\"--x=\\\"a\"",
        ),
        (
            &format!("ExecStart={R}/current/kbf --x=$HOME\n"),
            "does not carry",
        ),
        (
            &format!("ExecStart={R}/current/kbf --x=%H\n"),
            "does not carry",
        ),
        (&format!("ExecStart={R}/current/kbf ; rm\n"), "\";\""),
        (
            &format!("ExecStart={R}/current/kbf 'a'\n"),
            "does not carry",
        ),
    ];
    for (service, why) in cases {
        let err = parse(&unit(service), R).unwrap_err();
        assert!(err.contains(why), "{service:?}: {err}");
    }
}

/// A last line that ends with a backslash still counts.
#[test]
fn a_dangling_continuation_is_read_as_it_is() {
    let text = format!("[Service]\nExecStart={R}/current/kbf --mode=daemon \\");
    let unit = parse(&text, R).unwrap();
    assert_eq!(unit.args, ["--mode=daemon"]);
}
