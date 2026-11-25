use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};

use kbf_state::postgres::ConfigError;
use kbf_testkit::dir::TestDir;

use crate::file::tests::secret;
use crate::{AuthError, Context, FileFlaw, Skipped, Source, authenticate, ways, with_password};

const URL: &str = "postgres://kbf@db.example/kbf?sslmode=require";

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// A context with this euid, the role `kbf`, and the given PGPASSWORD and libpq file.
fn context(pgpassword: Option<&str>, passfile: Option<PathBuf>) -> Context {
    Context {
        pgpassword: pgpassword.map(OsString::from),
        passfile,
        euid: me(),
        user: Some("kbf".to_owned()),
    }
}

/// The password `url` (built by [`with_password`]) gives the client, as the client reads it.
fn client_password(url: &str) -> Vec<u8> {
    let config: postgres::Config = url.parse().expect("the client reads the URL");
    config.get_password().expect("a password").to_vec()
}

/// Catches a URL's password let through in either form the client reads (user information or a
/// `password` parameter), and a URL the client cannot use reported as anything but its
/// configuration error. The refusal text holds no part of the URL.
#[test]
fn a_url_that_carries_a_password_or_cannot_be_read_is_refused() {
    let ctx = context(Some("from-env"), None);
    for url in [
        "postgres://kbf:hunter2@db.example/kbf",
        "postgres://db.example/kbf?user=kbf&password=hunter2",
    ] {
        let refused = authenticate(url, None, &ctx).expect_err(url);
        assert_eq!(refused, AuthError::InUrl, "{url}");
        assert_eq!(
            refused.to_string(),
            "the URL carries a password, and a URL is never the place for one"
        );
    }
    let refused = authenticate("mysql://db.example/kbf", None, &ctx).expect_err("refused");
    assert_eq!(refused, AuthError::Url(ConfigError::NotPostgres));
    assert_eq!(refused.to_string(), "the URL is not usable");
}

/// Catches the order of the sources changed (the named file first, then PGPASSWORD, then
/// libpq's file), a source that has one passed over, and no source giving a password anyway.
#[test]
fn the_first_source_that_has_a_password_gives_it() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-order");
    let file = secret(&dir, "pw", b"from-file\n", 0o600);
    let pgpass = secret(
        &dir,
        "pgpass",
        b"db.example:5432:kbf:kbf:from-pgpass\n",
        0o600,
    );
    let both = context(Some("from-env"), Some(pgpass.clone()));
    let got = authenticate(URL, Some(&file), &both).expect("from the file");
    assert_eq!(got.source, Source::File(file.clone()));
    assert!(got.config.has_password());
    assert_eq!(got.skipped, None);
    let got = authenticate(URL, None, &both).expect("from PGPASSWORD");
    assert_eq!(got.source, Source::Pgpassword);
    assert!(got.config.has_password());
    let got = authenticate(URL, None, &context(None, Some(pgpass.clone()))).expect("pgpass");
    assert_eq!(
        got.source,
        Source::Passfile {
            path: pgpass,
            line: 1
        }
    );
    assert!(got.config.has_password());
    for none in [
        context(None, None),
        context(None, Some(dir.join("missing"))),
    ] {
        let got = authenticate(URL, None, &none).expect("no password");
        assert_eq!(got.source, Source::None);
        assert!(!got.config.has_password());
        assert_eq!(got.skipped, None);
    }
}

/// Catches a named password file that cannot be used passed over for another source (the
/// operator named it: a refusal says why), and the refusal text without the file or the flaw.
#[test]
fn a_named_password_file_that_cannot_be_used_is_a_refusal() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-named");
    let file = secret(&dir, "pw", b"hunter2\n", 0o644);
    let refused = authenticate(URL, Some(&file), &context(Some("env"), None)).expect_err("mode");
    assert_eq!(
        refused,
        AuthError::File {
            path: file.clone(),
            flaw: FileFlaw::Mode(0o644)
        }
    );
    assert_eq!(
        refused.to_string(),
        format!(
            "the password file {}: it has mode 0644; it must be 0600 or 0400",
            file.display()
        )
    );
}

/// Catches a password the client cannot send (not UTF-8) passed on mangled, from each source,
/// with a text that names the source and not the bytes.
#[test]
fn a_password_that_is_not_utf8_is_refused_by_its_source() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-utf8");
    let file = secret(&dir, "pw", b"bad\xff\n", 0o600);
    let refused = authenticate(URL, Some(&file), &context(None, None)).expect_err("file");
    assert_eq!(refused, AuthError::NotUtf8(Source::File(file.clone())));
    assert_eq!(
        refused.to_string(),
        format!(
            "the password in the password file {} is not UTF-8",
            file.display()
        )
    );
    let mut env = context(None, None);
    env.pgpassword = Some(OsString::from_vec(b"bad\xff".to_vec()));
    let refused = authenticate(URL, None, &env).expect_err("env");
    assert_eq!(
        refused.to_string(),
        "the password in PGPASSWORD is not UTF-8"
    );
    let pgpass = secret(&dir, "pgpass", b"*:*:*:*:bad\xff\n", 0o600);
    let refused =
        authenticate(URL, None, &context(None, Some(pgpass.clone()))).expect_err("pgpass");
    assert_eq!(
        refused.to_string(),
        format!("the password in {} line 1 is not UTF-8", pgpass.display())
    );
}

/// Catches libpq's file skipped without the caller hearing of it, and hosts whose lines give
/// different passwords connected with one of them.
#[test]
fn a_skipped_or_ambiguous_libpq_file_reaches_the_caller() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-libpq");
    let open = secret(&dir, "pgpass-open", b"*:*:*:*:pw\n", 0o644);
    let got = authenticate(URL, None, &context(None, Some(open.clone()))).expect("no password");
    assert_eq!(got.source, Source::None);
    assert!(!got.config.has_password());
    assert_eq!(
        got.skipped,
        Some(Skipped {
            path: open,
            why: "its group or others have access (mode 0644); it must be 0600 or less".to_owned()
        })
    );
    let pgpass = secret(
        &dir,
        "pgpass",
        b"a:5432:kbf:kbf:one\nb:5432:kbf:kbf:two\n",
        0o600,
    );
    let refused = authenticate(
        "postgres://kbf@a,b/kbf",
        None,
        &context(None, Some(pgpass.clone())),
    )
    .expect_err("hosts differ");
    assert_eq!(refused, AuthError::HostsDiffer(pgpass.clone()));
    assert_eq!(
        refused.to_string(),
        format!(
            "{}: the hosts of the URL match lines with different passwords, and one password \
             is offered to every host",
            pgpass.display()
        )
    );
}

/// Catches a password character that ends the parameter or the URL (`&`, `#`, `=`, `%`, `+`, a
/// space, a `:` or `@`) or is decoded into another, a URL with a query given a second `?`, and
/// the password shown in the configuration's `Debug` text. The password is every printable ASCII
/// character and one that is not ASCII, so no character the encoding must carry is left out.
#[test]
fn every_character_of_a_password_reaches_the_client_as_it_is() {
    let every: String = (b' '..=b'~').map(char::from).chain(['\u{e9}']).collect();
    for url in [
        "postgres://kbf@db.example/kbf",
        " postgres://db.example/kbf?user=kbf ",
    ] {
        let built = with_password(url, &every);
        assert_eq!(client_password(&built), every.as_bytes(), "{url}");
        assert_eq!(built.matches('?').count(), 1, "{url}");
    }
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-debug");
    let file = secret(&dir, "pw", format!("{every}\n").as_bytes(), 0o600);
    let got = authenticate(URL, Some(&file), &context(None, None)).expect("authenticated");
    let shown = format!("{got:?}");
    // A run of the password, as it is and as it is encoded, appears nowhere in the text.
    assert!(!shown.contains("!\"#$%") && !shown.contains("%21%22%23"));
}

/// Catches PGPASSFILE not taking the place of `~/.pgpass`, an empty variable taken as set (libpq
/// takes it as unset), and PGPASSWORD shown by `Debug`.
#[test]
fn the_context_reads_the_variables_as_libpq_does() {
    let vars = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    };
    let all = Context::from_env(
        vars(&[
            ("PGPASSWORD", "pw"),
            ("PGPASSFILE", "/etc/kbf/pgpass"),
            ("HOME", "/var/lib/kbf-k"),
        ]),
        7,
        Some("k".to_owned()),
    );
    assert_eq!(
        all,
        Context {
            pgpassword: Some(OsString::from("pw")),
            passfile: Some(PathBuf::from("/etc/kbf/pgpass")),
            euid: 7,
            user: Some("k".to_owned()),
        }
    );
    assert_eq!(
        format!("{all:?}"),
        "Context { pgpassword: Some(\"..\"), passfile: Some(\"/etc/kbf/pgpass\"), euid: 7, \
         user: Some(\"k\") }"
    );
    let home = Context::from_env(
        vars(&[
            ("PGPASSWORD", ""),
            ("PGPASSFILE", ""),
            ("HOME", "/var/lib/kbf-k"),
        ]),
        7,
        None,
    );
    assert_eq!(home.pgpassword, None);
    assert_eq!(
        home.passfile.as_deref(),
        Some(Path::new("/var/lib/kbf-k/.pgpass"))
    );
    let bare = Context::from_env(vars(&[("HOME", "")]), 7, None);
    assert_eq!(bare.passfile, None);
    assert!(format!("{bare:?}").contains("pgpassword: None"));
}

/// Catches the process's context taken with another uid than its own (the owner check would
/// then judge the wrong user).
#[test]
fn the_process_context_has_this_process_s_uid() {
    assert_eq!(Context::of_process().euid, me());
}

/// Catches a source's text that does not say where the password came from, or a refusal that
/// does not name every way a password may be given.
#[test]
fn the_texts_name_the_sources_and_the_ways() {
    assert_eq!(Source::None.to_string(), "no password");
    assert_eq!(
        Source::File(PathBuf::from("/etc/kbf/pg")).to_string(),
        "the password file /etc/kbf/pg"
    );
    assert_eq!(Source::Pgpassword.to_string(), "PGPASSWORD");
    assert_eq!(
        Source::Passfile {
            path: PathBuf::from("/root/.pgpass"),
            line: 3
        }
        .to_string(),
        "/root/.pgpass line 3"
    );
    assert_eq!(
        ways(Some("--state-store-password-file")),
        "give it in a file of mode 0600 named by --state-store-password-file, in PGPASSWORD, or \
         a line of ~/.pgpass (PGPASSFILE names another file)"
    );
    assert_eq!(
        ways(None),
        "give it in PGPASSWORD, or a line of ~/.pgpass (PGPASSFILE names another file)"
    );
}
