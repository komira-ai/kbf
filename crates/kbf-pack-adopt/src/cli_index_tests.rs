//! `--pack-index`'s password: the URL carries none, PGPASSWORD or libpq's file gives it.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use kbf_pg_auth::Context;
use kbf_testkit::dir::TestDir;

use super::{BAD_INDEX_URL, INDEX_PASSWORD, index_config};
use crate::error::Error;

const URL: &str =
    "postgres://db.example/kbf?user=kbf&sslmode=verify-full&sslrootcert=/etc/kbf/ca.pem";

fn context(pgpassword: Option<&str>, passfile: Option<PathBuf>) -> Context {
    Context {
        pgpassword: pgpassword.map(OsString::from),
        passfile,
        euid: 0,
        user: None,
    }
}

fn refusal(url: &str, context: &Context) -> String {
    match index_config(url, context) {
        Err(Error::Refused(why)) => why,
        Err(other) => panic!("not a refusal: {other}"),
        Ok(_) => panic!("{url}: not refused"),
    }
}

/// Catches PGPASSWORD or a libpq file line not reaching the index's connections, a libpq file
/// libpq would skip used anyway (or a refusal instead of a warning), and no source inventing a
/// password.
#[test]
fn the_index_takes_its_password_from_pgpassword_or_the_libpq_file() {
    assert!(
        index_config(URL, &context(Some("hunter2"), None))
            .expect("env")
            .has_password()
    );
    let dir = TestDir::under(std::env::temp_dir(), "pack-index-password");
    let pgpass = dir.join("pgpass");
    std::fs::write(&pgpass, "db.example:5432:kbf:kbf:hunter2\n").expect("write");
    for (mode, used) in [(0o600, true), (0o644, false)] {
        std::fs::set_permissions(&pgpass, std::fs::Permissions::from_mode(mode)).expect("chmod");
        let config = index_config(URL, &context(None, Some(pgpass.clone()))).expect("usable");
        assert_eq!(config.has_password(), used, "{mode:o}");
    }
    assert!(
        !index_config(URL, &context(None, None))
            .expect("none")
            .has_password()
    );
}

/// Catches a password in the URL let through (the command line is world-readable), a URL the
/// client cannot use quoted back, and a libpq file whose lines disagree for the URL's hosts
/// taken; each with its whole text, which never holds the URL or the password.
#[test]
fn a_password_in_the_url_or_one_that_cannot_be_given_is_refused() {
    let none = context(None, None);
    assert_eq!(
        refusal("postgres://kbf:hunter2@db.example/kbf", &none),
        INDEX_PASSWORD
    );
    assert_eq!(
        INDEX_PASSWORD,
        "--pack-index: the URL must carry no password (a command line is world-readable): give \
         it in PGPASSWORD, or a line of ~/.pgpass (PGPASSFILE names another file); a Unix socket \
         with peer authentication, as in postgres:///kbf?host=/run/postgresql, needs none"
    );
    assert_eq!(refusal("mysql://db.example/kbf", &none), BAD_INDEX_URL);
    let dir = TestDir::under(std::env::temp_dir(), "pack-index-hosts");
    let pgpass = dir.join("pgpass");
    std::fs::write(&pgpass, "a:5432:kbf:kbf:one\nb:5432:kbf:kbf:two\n").expect("write");
    std::fs::set_permissions(&pgpass, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(
        refusal(
            "postgres://kbf@a,b/kbf",
            &context(None, Some(pgpass.clone()))
        ),
        format!(
            "--pack-index: {}: the hosts of the URL match lines with different passwords, and \
             one password is offered to every host",
            pgpass.display()
        )
    );
}
