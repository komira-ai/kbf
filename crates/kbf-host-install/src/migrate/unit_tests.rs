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
        (
            &format!("ExecStart={R}/current/kbf --x=a\\b\n"),
            "does not carry",
        ),
        (&format!("ExecStart={R}/current/kbf ; rm\n"), "\";\""),
        (
            &format!("ExecStart={R}/current/kbf --x=a;b\n"),
            "does not carry",
        ),
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

/// A cold-store URL with a query (`?` and `&`) is one plain word and is read unchanged.
#[test]
fn a_cold_store_url_with_a_query_is_carried_as_it_is() {
    let flag = "--cold-store=s3://bucket-name/prefix/?endpoint=http://127.0.0.1:30900&conditional-put=true";
    let text =
        format!("[Service]\nExecStart={R}/current/kbf --mode=storage {flag} --node-id st1\n");
    let unit = parse(&text, R).unwrap();
    assert_eq!(unit.args, ["--mode=storage", flag, "--node-id", "st1"]);
}

fn flags_of(args: &str, mode: &str) -> Result<Vec<String>, String> {
    parse(&unit(&format!("ExecStart={R}/current/kbf {args}")), R)
        .unwrap()
        .flags(mode, "kbf-x.service")
}

/// The mode is named by `--mode=` alone on the packaged unit: the older first-word spelling is
/// dropped when it is the unit's mode (the package's kbf refuses the word after `--mode=`), a
/// `--mode` of the unit's mode is kept once, and a unit that names none gets one.
#[test]
fn the_mode_is_named_by_the_flag_alone() {
    let words = |w: &[&str]| w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let cold = "--cold-store=file:///var/lib/kbf/cold";
    assert_eq!(
        flags_of(&format!("storage {cold}"), "storage").unwrap(),
        words(&["--mode=storage", cold])
    );
    assert_eq!(
        flags_of(&format!("storage --mode=storage {cold}"), "storage").unwrap(),
        words(&["--mode=storage", cold])
    );
    assert_eq!(
        flags_of(&format!("--mode=storage {cold}"), "storage").unwrap(),
        words(&["--mode=storage", cold])
    );
    assert_eq!(
        flags_of(&format!("--mode storage {cold}"), "storage").unwrap(),
        words(&["--mode", "storage", cold])
    );
    assert_eq!(
        flags_of(cold, "storage").unwrap(),
        words(&["--mode=storage", cold])
    );
    // Only the first word is the old spelling; a flag whose name starts with `--mode` is not.
    assert_eq!(
        flags_of("storage --mode-x=1", "storage").unwrap(),
        words(&["--mode=storage", "--mode-x=1"])
    );
}

/// A mode the unit's name does not say, by word or by flag, stops the move.
#[test]
fn another_mode_is_refused_by_word_and_by_flag() {
    for args in [
        "daemon --cold-store=x",
        "--mode=daemon --cold-store=x",
        "--mode daemon --cold-store=x",
        "storage --mode=daemon",
        "bogus",
    ] {
        let why = flags_of(args, "storage").unwrap_err();
        assert!(
            why.contains("not the mode of kbf-x.service"),
            "{args}: {why}"
        );
    }
}
