//! The password of a Postgres connection: from a file kbf is told of, from `PGPASSWORD` or from
//! libpq's password file, and never from the URL.
//!
//! kbf's Postgres URLs are on command lines and in files others may read (`installer.toml` is
//! world-readable), so a URL that carries a password, as user information (`user:secret@`) or a
//! `password` parameter, is refused ([`AuthError::InUrl`], whose text names the ways below). The
//! password comes instead from the first of these that has one:
//!
//! 1. **The password file the program names** (`--state-store-password-file`, the
//!    `database_password_file` key of `installer.toml`). It is checked as kbf's other secret files
//!    are, on the open file: a regular file, not a symbolic link, owned by the effective user (or
//!    any owner when that user is root, who runs the installer and the boot check for a service
//!    that owns the file), mode 0600 or 0400, at most [`MAX_PASSWORD_FILE_BYTES`]. It holds the
//!    password and nothing else; one line ending after it is dropped. A file that is named and
//!    cannot be used is a refusal ([`AuthError::File`]), never a reason to look further.
//! 2. **`PGPASSWORD`**, as libpq takes it. An empty one is none.
//! 3. **libpq's password file**: `PGPASSFILE`, else `.pgpass` in `HOME`. Its lines are
//!    `host:port:database:user:password`, `*` matches any value, `\` escapes `:` and `\`, lines
//!    starting with `#` are comments, and the first line that matches wins. A host is matched by
//!    its name, a Unix socket by its directory (`/run/postgresql`); a port the URL leaves out is
//!    5432; a database it leaves out is the role's name; a role it leaves out is the effective
//!    user's name, as the client uses. Like libpq, a file that is not a regular file or that
//!    its group or others can read or write is skipped, and the caller is told why
//!    ([`Authenticated::skipped`]); a missing file is no password. A URL naming several hosts
//!    takes a password only when every host's line gives the same one
//!    ([`AuthError::HostsDiffer`]): the client offers one password to each.
//!
//! With none of them the connection goes without a password, as it did before: peer
//! authentication over a Unix socket, or a server that refuses the login.
//!
//! The password reaches the client as a percent-encoded `password` parameter of the URL, which is
//! parsed again ([`PostgresConfig`] has no other way in): the original URL text is not changed,
//! and no message here quotes the URL or the password.
//!
//! TLS is the URL's own: `sslmode` and `sslrootcert` ([`kbf_state::postgres::TlsMode`]); a
//! password sent to a server over a network should go with `sslmode=verify-full`.
//!
//! The environment this reads (`PGPASSWORD`, `PGPASSFILE`, `HOME`) is read once, by
//! [`Context::of_process`]; a test builds a [`Context`] of its own.
//!
//! Not a source: an AWS RDS IAM authentication token. A token is good for 15 minutes and the
//! store's pool opens connections long after the start, so it needs a password asked for at
//! each connection, which `PostgresConfig` does not offer, and AWS credentials to sign it with.
//! It would be a fourth [`Source`], made where [`authenticate`] chooses one.

#![forbid(unsafe_code)]

mod file;
mod pgpass;

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use kbf_state::postgres::{ConfigError, PostgresConfig};
use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use zeroize::Zeroizing;

pub use file::{FileFlaw, MAX_PASSWORD_FILE_BYTES};
pub use pgpass::Skipped;

/// What a refusal of a password in a URL says to do instead, for a program whose password file
/// is given by `file` (a flag or a key), or `None` for one that takes no file.
#[must_use]
pub fn ways(file: Option<&str>) -> String {
    let libpq = "PGPASSWORD, or a line of ~/.pgpass (PGPASSFILE names another file)";
    match file {
        Some(file) => {
            format!("give it in a file of mode 0600 named by {file}, in {libpq}")
        }
        None => format!("give it in {libpq}"),
    }
}

/// What a password is looked up with: the environment's part and who this process is.
#[derive(Clone, PartialEq, Eq)]
pub struct Context {
    /// `PGPASSWORD`.
    pub pgpassword: Option<OsString>,
    /// libpq's password file: `PGPASSFILE`, else `.pgpass` in `HOME`; `None` when neither is set.
    pub passfile: Option<PathBuf>,
    /// The effective user id: the owner a password file must have.
    pub euid: u32,
    /// The effective user's name: the role the client logs in as when the URL names none.
    pub user: Option<String>,
}

impl fmt::Debug for Context {
    // PGPASSWORD is a password; say only whether it is set.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("pgpassword", &self.pgpassword.as_ref().map(|_| ".."))
            .field("passfile", &self.passfile)
            .field("euid", &self.euid)
            .field("user", &self.user)
            .finish()
    }
}

impl Context {
    /// This process's: the environment's `PGPASSWORD`, `PGPASSFILE` and `HOME` (empty ones are
    /// unset, as libpq takes them), the effective uid, and the user name the client would use.
    #[must_use]
    pub fn of_process() -> Context {
        Context::from_env(
            |name| std::env::var_os(name),
            rustix::process::geteuid().as_raw(),
            whoami::username().ok(),
        )
    }

    /// The context of the variables `var` gives, with this euid and user name.
    #[must_use]
    pub fn from_env(
        var: impl Fn(&str) -> Option<OsString>,
        euid: u32,
        user: Option<String>,
    ) -> Context {
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        let passfile = set("PGPASSFILE")
            .map(PathBuf::from)
            .or_else(|| set("HOME").map(|home| Path::new(&home).join(".pgpass")));
        Context {
            pgpassword: set("PGPASSWORD"),
            passfile,
            euid,
            user,
        }
    }
}

/// Where the password came from. Its text names the source and never holds the password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// No source had one.
    None,
    /// The password file the program was given.
    File(PathBuf),
    /// `PGPASSWORD`.
    Pgpassword,
    /// A line of libpq's password file.
    Passfile {
        /// The file.
        path: PathBuf,
        /// The line, from 1.
        line: usize,
    },
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("no password"),
            Self::File(path) => write!(f, "the password file {}", path.display()),
            Self::Pgpassword => f.write_str("PGPASSWORD"),
            Self::Passfile { path, line } => write!(f, "{} line {line}", path.display()),
        }
    }
}

/// Why no configuration came of a URL and its password sources. No text holds the URL or the
/// password; the caller words [`AuthError::Url`] itself, since the parser's text may quote it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The URL is not one the client can use.
    #[error("the URL is not usable")]
    Url(ConfigError),
    /// The URL carries a password.
    #[error("the URL carries a password, and a URL is never the place for one")]
    InUrl,
    /// The password file the program was given cannot be used.
    #[error("the password file {path}: {flaw}")]
    File {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        flaw: FileFlaw,
    },
    /// The password is not UTF-8, which the client cannot send.
    #[error("the password in {0} is not UTF-8")]
    NotUtf8(Source),
    /// The hosts of the URL match lines of libpq's password file with different passwords (or
    /// some match none), and the client offers one password to every host.
    #[error(
        "{0}: the hosts of the URL match lines with different passwords, and one password is \
         offered to every host"
    )]
    HostsDiffer(PathBuf),
}

/// A configuration to connect with and what it took its password from.
#[derive(Clone, Debug)]
pub struct Authenticated {
    /// The configuration of the URL, with the password when a source had one.
    pub config: PostgresConfig,
    /// Where the password came from.
    pub source: Source,
    /// libpq's password file, when it was there and skipped: for the caller to log.
    pub skipped: Option<Skipped>,
}

/// The configuration of `url` with the password of the first source that has one:
/// `password_file`, then `PGPASSWORD`, then libpq's password file (see the crate's text).
///
/// # Errors
/// [`AuthError::Url`] when the URL does not parse, [`AuthError::InUrl`] when it carries a
/// password; [`AuthError::File`] when `password_file` cannot be used; [`AuthError::NotUtf8`]
/// and [`AuthError::HostsDiffer`] for a password the client cannot be given.
pub fn authenticate(
    url: &str,
    password_file: Option<&Path>,
    context: &Context,
) -> Result<Authenticated, AuthError> {
    let plain = PostgresConfig::parse(url).map_err(AuthError::Url)?;
    if plain.has_password() {
        return Err(AuthError::InUrl);
    }
    let (found, skipped) = find(url, password_file, context)?;
    let Some((password, source)) = found else {
        return Ok(Authenticated {
            config: plain,
            source: Source::None,
            skipped,
        });
    };
    let Ok(password) = std::str::from_utf8(&password) else {
        return Err(AuthError::NotUtf8(source));
    };
    let config = PostgresConfig::parse(&with_password(url, password)).map_err(AuthError::Url)?;
    Ok(Authenticated {
        config,
        source,
        skipped,
    })
}

/// A password and where it came from.
type Found = (Zeroizing<Vec<u8>>, Source);

/// The first source's password, and libpq's file when it was skipped.
fn find(
    url: &str,
    password_file: Option<&Path>,
    context: &Context,
) -> Result<(Option<Found>, Option<Skipped>), AuthError> {
    if let Some(path) = password_file {
        let password = file::read(path, context.euid).map_err(|flaw| AuthError::File {
            path: path.to_owned(),
            flaw,
        })?;
        return Ok((Some((password, Source::File(path.to_owned()))), None));
    }
    if let Some(value) = &context.pgpassword {
        let bytes = Zeroizing::new(value.as_encoded_bytes().to_vec());
        return Ok((Some((bytes, Source::Pgpassword)), None));
    }
    let Some(path) = &context.passfile else {
        return Ok((None, None));
    };
    let targets = pgpass::targets(url, context.user.as_deref());
    match pgpass::lookup(path, &targets) {
        Ok(found) => Ok((found, None)),
        Err(pgpass::Miss::Skipped(skipped)) => Ok((None, Some(skipped))),
        Err(pgpass::Miss::HostsDiffer) => Err(AuthError::HostsDiffer(path.clone())),
    }
}

/// `url` with `password` as its `password` parameter, every byte but letters and digits
/// percent-encoded, so that no character of the password ends the parameter or the URL.
fn with_password(url: &str, password: &str) -> Zeroizing<String> {
    let url = url.trim();
    let separator = if url.contains('?') { '&' } else { '?' };
    let encoded = percent_encode(password.as_bytes(), NON_ALPHANUMERIC);
    Zeroizing::new(format!("{url}{separator}password={encoded}"))
}

#[cfg(test)]
mod tests;
