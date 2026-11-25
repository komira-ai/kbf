//! The password of `installer.toml`'s database (`installer_database`): no server.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use kbf_pg_auth::{AuthError, Context, FileFlaw, Source};

use super::{DatabaseError, installer_database};

const URL: &str = "postgres://db.example/kbf?user=kbf&sslmode=require";

/// A context with this euid, the given PGPASSWORD and no libpq file.
fn context(pgpassword: Option<&str>) -> Context {
    Context {
        pgpassword: pgpassword.map(OsString::from),
        passfile: None,
        euid: rustix::process::geteuid().as_raw(),
        user: None,
    }
}

/// A password file of `mode` in a scratch directory of the test's own.
fn password_file(name: &str, mode: u32) -> PathBuf {
    let dir = kbf_host::testkit::scratch(&format!("database-password-{name}"));
    let file = dir.join("pw");
    std::fs::write(&file, "hunter2\n").expect("written");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).expect("chmod");
    file
}

fn body(file: &str) -> String {
    format!("database = \"{URL}\"\ndatabase_password_file = \"{file}\"\n")
}

/// Catches `database_password_file` not read (the installer would log in without the password),
/// PGPASSWORD not taken when the key is missing, the key's file passed over for PGPASSWORD, and a
/// file without the `database` key given a database.
#[test]
fn the_password_comes_from_the_key_s_file_else_the_environment() {
    let file = password_file("key", 0o600);
    let found = installer_database(
        "installer.toml",
        &body(&file.display().to_string()),
        &context(Some("env")),
    )
    .expect("usable")
    .expect("a database");
    assert_eq!(found.source, Source::File(file));
    assert!(found.config.has_password());
    let found = installer_database(
        "installer.toml",
        &format!("database = \"{URL}\"\n"),
        &context(Some("env")),
    )
    .expect("usable")
    .expect("a database");
    assert_eq!(found.source, Source::Pgpassword);
    let found = installer_database(
        "installer.toml",
        &format!("database = \"{URL}\"\n"),
        &context(None),
    )
    .expect("usable")
    .expect("a database");
    assert_eq!(found.source, Source::None);
    assert!(!found.config.has_password());
    let none = installer_database(
        "installer.toml",
        "release_source = \"bundled\"\n",
        &context(None),
    );
    assert!(matches!(none, Ok(None)));
}

/// Catches a password file others can read used, a key that is not an absolute path (or not a
/// string) taken as a path, and a URL that carries a password let through; each refusal's whole
/// text, which names the file and never the password or the URL.
#[test]
fn a_password_that_cannot_be_had_or_sits_in_the_url_is_refused() {
    let open = password_file("open", 0o644);
    let refused = installer_database(
        "installer.toml",
        &body(&open.display().to_string()),
        &context(None),
    )
    .expect_err("a file others can read");
    assert_eq!(
        refused,
        DatabaseError::PasswordSource {
            path: "installer.toml".to_owned(),
            why: AuthError::File {
                path: open.clone(),
                flaw: FileFlaw::Mode(0o644)
            },
        }
    );
    assert_eq!(
        refused.to_string(),
        format!(
            "the password of the `database` in installer.toml: the password file {}: it has mode \
             0644; it must be 0600 or 0400",
            open.display()
        )
    );
    for key in ["\"pw\"", "5"] {
        let text = format!("database = \"{URL}\"\ndatabase_password_file = {key}\n");
        let refused = installer_database("installer.toml", &text, &context(None)).expect_err(key);
        assert_eq!(
            refused,
            DatabaseError::PasswordFileKey("installer.toml".to_owned())
        );
        assert_eq!(
            refused.to_string(),
            "the `database_password_file` in installer.toml is not an absolute path"
        );
    }
    let refused = installer_database(
        "installer.toml",
        "database = \"postgres://kbf:hunter2@db.example/kbf\"\n",
        &context(None),
    )
    .expect_err("a password in the URL");
    assert_eq!(
        refused.to_string(),
        "the `database` URL in installer.toml carries a password, and the file is world-readable: \
         give it in a file of mode 0600 named by database_password_file, in PGPASSWORD, or a line \
         of ~/.pgpass (PGPASSFILE names another file); peer authentication needs none"
    );
}
