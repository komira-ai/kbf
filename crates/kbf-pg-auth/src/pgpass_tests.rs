use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;

use super::{Miss, Skipped, Target, field, lookup, targets, unescape};
use crate::Source;
use crate::file::tests::secret;

fn target(host: &str, port: &str, database: &str, user: &str) -> Target {
    Target {
        host: host.as_bytes().to_vec(),
        port: port.as_bytes().to_vec(),
        database: database.as_bytes().to_vec(),
        user: user.as_bytes().to_vec(),
    }
}

/// The password a lookup in `path` gives `targets`, as text, with its line.
fn found(path: &Path, targets: &[Target]) -> Option<(String, usize)> {
    lookup(path, targets)
        .expect("looked up")
        .map(|(password, source)| {
            let Source::Passfile { line, .. } = source else {
                panic!("not a line of the file: {source:?}")
            };
            (String::from_utf8(password.to_vec()).expect("UTF-8"), line)
        })
}

/// Catches a Unix socket matched by anything but its directory, a missing port that is not
/// 5432, a missing database that is not the role's name, a missing role that is not the
/// effective user's, and the TLS parameters (which the client does not read) making the URL
/// unreadable here.
#[test]
fn a_url_names_the_servers_a_line_is_matched_against() {
    assert_eq!(
        targets(
            "postgres:///kbf?host=/run/postgresql&user=kbf-server",
            Some("root")
        ),
        [target("/run/postgresql", "5432", "kbf", "kbf-server")]
    );
    assert_eq!(
        targets(
            " postgres://kbf@db.example:6432/state?sslmode=verify-full&sslrootcert=%2Fca.pem ",
            None
        ),
        [target("db.example", "6432", "state", "kbf")]
    );
    assert_eq!(
        targets("postgres://db.example", Some("me")),
        [target("db.example", "5432", "me", "me")]
    );
    assert_eq!(
        targets("postgres://db.example?&sslmode=require&", None),
        [target("db.example", "5432", "", "")]
    );
}

/// Catches a second host given the first host's port, a host written without a port given
/// another than 5432, and a URL the client cannot read giving servers of its own invention.
#[test]
fn every_host_of_a_url_is_a_server_with_its_port() {
    assert_eq!(
        targets("postgres://u@a:1,b:2/db", None),
        [target("a", "1", "db", "u"), target("b", "2", "db", "u")]
    );
    assert_eq!(
        targets("postgres://u@a,b:7/db", None),
        [target("a", "5432", "db", "u"), target("b", "7", "db", "u")]
    );
    assert_eq!(targets("postgres://h/db?bogus=1", Some("u")), []);
}

/// Catches libpq's field rules broken: `*` matching only as the whole field, `\` escaping the
/// next character (a `:` or a `\`), a field that is only a prefix of the value or longer than
/// it, and a line that ends inside a field.
#[test]
fn a_field_matches_as_libpq_matches_it() {
    assert_eq!(field(b"*:rest", b"anything"), Some(&b"rest"[..]));
    assert_eq!(field(b"db:rest", b"db"), Some(&b"rest"[..]));
    assert_eq!(field(b"\\:\\:1:rest", b"::1"), Some(&b"rest"[..]));
    assert_eq!(field(b"a\\\\b:rest", b"a\\b"), Some(&b"rest"[..]));
    assert_eq!(field(b"*x:rest", b"*x"), Some(&b"rest"[..]));
    assert_eq!(field(b"*x:rest", b"y"), None);
    assert_eq!(field(b"db:rest", b"db2"), None);
    assert_eq!(field(b"db2:rest", b"db"), None);
    assert_eq!(field(b"db", b"db"), None);
    assert_eq!(field(b"db\\", b"db"), None);
    assert_eq!(field(b":rest", b""), Some(&b"rest"[..]));
}

/// Catches a password's escapes left in (`\:` is a `:`, `\\` a `\`), a password that runs on past
/// an unescaped `:`, and a last lone `\` dropped (libpq keeps it).
#[test]
fn a_password_field_is_unescaped_up_to_its_first_bare_colon() {
    assert_eq!(unescape(b"plain"), b"plain");
    assert_eq!(unescape(b"a\\:b\\\\c"), b"a:b\\c");
    assert_eq!(unescape(b"pw:more"), b"pw");
    assert_eq!(unescape(b"end\\"), b"end\\");
    assert_eq!(unescape(b""), b"");
}

/// Catches comments, blank lines or a CRLF ending taken as lines, a later line winning over the
/// first that matches, a wildcard line not matching, the line number off by one, and a file
/// with no matching line or no file at all being anything but no password.
#[test]
fn the_first_line_that_matches_gives_the_password() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-pgpass-lines");
    let text = "# kbf's databases\r\n\
                \n\
                other.example:5432:kbf:kbf:not-this\n\
                db.example:5432:kbf:kbf:first\\:one\r\n\
                db.example:5432:kbf:kbf:second\n\
                *:*:*:reader:wild\n";
    let path = secret(&dir, "pgpass", text.as_bytes(), 0o600);
    let kbf = target("db.example", "5432", "kbf", "kbf");
    assert_eq!(found(&path, &[kbf]), Some(("first:one".to_owned(), 4)));
    let reader = target("anywhere", "1", "any", "reader");
    assert_eq!(found(&path, &[reader]), Some(("wild".to_owned(), 6)));
    let nobody = target("db.example", "5432", "kbf", "nobody");
    assert_eq!(found(&path, std::slice::from_ref(&nobody)), None);
    assert_eq!(found(&path, &[]), None);
    assert_eq!(found(&dir.join("missing"), &[nobody]), None);
}

/// Catches hosts that do not agree given a password anyway (the client offers one password to
/// every host): different passwords, or a password for some hosts and none for others.
#[test]
fn several_hosts_take_a_password_only_when_their_lines_agree() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-pgpass-hosts");
    let text = "a:5432:db:u:same\nb:5432:db:u:same\nc:5432:db:u:other\n";
    let path = secret(&dir, "pgpass", text.as_bytes(), 0o600);
    let [a, b, c, d] = ["a", "b", "c", "d"].map(|host| target(host, "5432", "db", "u"));
    assert_eq!(found(&path, &[a.clone(), b]), Some(("same".to_owned(), 1)));
    for hosts in [
        vec![a.clone(), c],
        vec![a.clone(), d.clone()],
        vec![d.clone(), a],
    ] {
        assert_eq!(lookup(&path, &hosts).map(|_| ()), Err(Miss::HostsDiffer));
    }
    assert_eq!(found(&path, &[d]), None);
    // One password, spelt with an escape on one line and followed by more on the other.
    let spelt = secret(
        &dir,
        "pgpass-spelt",
        b"a:5432:db:u:pw:more\nb:5432:db:u:p\\w\n",
        0o600,
    );
    let [a, b] = ["a", "b"].map(|host| target(host, "5432", "db", "u"));
    assert_eq!(found(&spelt, &[a, b]), Some(("pw".to_owned(), 1)));
}

/// Catches an empty password field taken as anything but the empty password libpq sends, and a
/// `\r` before the line ending kept in the password.
#[test]
fn an_empty_password_field_is_the_empty_password() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-pgpass-empty");
    let path = secret(&dir, "pgpass", b"h:5432:db:u:\r\nh:5432:db:v:pw\r\n", 0o600);
    assert_eq!(
        found(&path, &[target("h", "5432", "db", "u")]),
        Some((String::new(), 1))
    );
    assert_eq!(
        found(&path, &[target("h", "5432", "db", "v")]),
        Some(("pw".to_owned(), 2))
    );
}

/// Catches a file its group or others can reach being read (libpq skips it, and says so), a
/// directory or other non-file read, a file that cannot be opened being an error rather than a
/// skip, and the reason not reaching the caller in full.
#[test]
fn a_file_libpq_would_skip_is_skipped_with_its_reason() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-pgpass-skip");
    let any = [target("h", "5432", "db", "u")];
    let skipped = |path: &PathBuf| match lookup(path, &any) {
        Err(Miss::Skipped(skipped)) => skipped,
        other => panic!("not skipped: {other:?}"),
    };
    for mode in [0o640, 0o604, 0o644] {
        let path = secret(&dir, &format!("pgpass-{mode:o}"), b"*:*:*:*:pw\n", mode);
        let got = skipped(&path);
        assert_eq!(
            got,
            Skipped {
                path: path.clone(),
                why: format!(
                    "its group or others have access (mode {mode:04o}); it must be 0600 or less"
                ),
            }
        );
        assert_eq!(
            got.to_string(),
            format!(
                "{} is skipped, as libpq skips it: its group or others have access (mode \
                 {mode:04o}); it must be 0600 or less",
                path.display()
            )
        );
    }
    let path = secret(&dir, "pgpass-400", b"*:*:*:*:pw\n", 0o400);
    assert_eq!(found(&path, &any), Some(("pw".to_owned(), 1)));
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).expect("mkdir");
    assert_eq!(skipped(&sub).why, "it is not a regular file");
    let under_a_file = path.join("pgpass");
    let enotdir = std::io::Error::from_raw_os_error(rustix::io::Errno::NOTDIR.raw_os_error());
    assert_eq!(
        skipped(&under_a_file).why,
        format!("it cannot be read: {enotdir}")
    );
}
