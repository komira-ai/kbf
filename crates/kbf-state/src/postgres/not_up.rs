//! Whether a failure to open the store is one a wait can cure, told from the client's typed
//! errors (`io::ErrorKind`, SQLSTATE), never from a message.
//!
//! A host that restarts starts Postgres and the process that needs it together; the process
//! is often first. What it meets then is the server not being up yet, which is over in
//! seconds, and that is told apart from a refusal, which no wait cures and which a person
//! has to read (a URL, a login, a database, a schema).

use std::error::Error as _;
use std::io;

use postgres::error::SqlState;

use super::Fail;
use crate::StoreError;

/// Why [`super::PostgresStore::try_open`] failed, by what a caller does about it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// The server is not there yet, and trying again later can succeed: nothing listens
    /// (connection refused), the connection was reset or aborted, the Unix socket does not
    /// exist yet (Postgres makes it when it starts), the connect timed out, or the server
    /// answered SQLSTATE 57P03 (`cannot_connect_now`: the database system is starting up, in
    /// recovery or shutting down).
    #[error(transparent)]
    NotUp(StoreError),
    /// Everything else, and so a start that waiting does not change: a URL the client cannot
    /// use, a refused login (class 28), a database or role that does not exist, a server that
    /// does not flush commits, a schema this build refuses, any other SQLSTATE, and any
    /// failure that is not an I/O error of the connection.
    #[error(transparent)]
    Refused(StoreError),
}

impl OpenError {
    /// Whether waiting and opening again can succeed.
    #[must_use]
    pub fn is_not_up(&self) -> bool {
        matches!(self, OpenError::NotUp(_))
    }

    /// The failure, as [`super::PostgresStore::open`] reports it.
    #[must_use]
    pub fn into_store(self) -> StoreError {
        match self {
            OpenError::NotUp(e) | OpenError::Refused(e) => e,
        }
    }
}

impl From<Fail> for OpenError {
    fn from(fail: Fail) -> OpenError {
        let not_up = match &fail {
            Fail::Pg(err) => err.code().map_or_else(|| io_cause(err), starting_up),
            // A schema or durability refusal came from a server that answered.
            Fail::Refused(_) => false,
        };
        let error = fail.into_store();
        if not_up {
            OpenError::NotUp(error)
        } else {
            OpenError::Refused(error)
        }
    }
}

/// Whether the server says it cannot connect now: `57P03` (starting up, in recovery or shutting
/// down), the only server answer that a wait cures. The other class 57 codes (`57P01` an
/// administrator's shutdown of the session, `57P02` a crash) are a server going away, which a
/// start that finds it at once should not outwait, and class 28 and `3D000` are a person's to
/// fix.
fn starting_up(code: &SqlState) -> bool {
    *code == SqlState::CANNOT_CONNECT_NOW
}

/// Whether the error has no SQLSTATE and an `io::Error` in its causes of a kind that means
/// the server is not listening yet.
fn io_cause(err: &postgres::Error) -> bool {
    let mut cause = err.source();
    while let Some(e) = cause {
        if let Some(io) = e.downcast_ref::<io::Error>() {
            return not_listening(io.kind());
        }
        cause = e.source();
    }
    false
}

/// The I/O kinds of a connect to a server that is not there yet: nothing listens
/// (`ConnectionRefused`), a socket that dropped (`ConnectionReset`, `ConnectionAborted`), a
/// Unix socket path that is not made yet (`NotFound`), and a connect that timed out
/// (`TimedOut`). `PermissionDenied` is not among them: the socket is there and will not let
/// this user in, which is a person's to fix, as is a name that does not resolve.
fn not_listening(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotFound
            | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::net::UnixListener;

    use kbf_testkit::dir::TestDir;
    use postgres::error::SqlState;

    use super::{OpenError, not_listening, starting_up};
    use crate::postgres::PostgresConfig;
    use crate::postgres::PostgresStore;

    fn open(url: &str) -> OpenError {
        let config = PostgresConfig::parse(url).expect("parses");
        PostgresStore::try_open(&config)
            .err()
            .expect("no server answers")
    }

    /// Only 57P03 among the server's answers is "not up yet"; a login refusal, a missing
    /// database, a server shutting down and an unknown code are not.
    #[test]
    fn only_the_starting_up_sqlstate_is_not_up() {
        assert!(starting_up(&SqlState::CANNOT_CONNECT_NOW));
        for refused in [
            SqlState::INVALID_PASSWORD,
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            SqlState::INVALID_CATALOG_NAME,
            SqlState::INSUFFICIENT_PRIVILEGE,
            SqlState::ADMIN_SHUTDOWN,
            SqlState::CRASH_SHUTDOWN,
            SqlState::from_code("XX000"),
        ] {
            assert!(!starting_up(&refused), "{refused:?}");
        }
    }

    /// The I/O kinds that mean the server is not listening are told from those a wait does
    /// not cure.
    #[test]
    fn the_io_kinds_of_a_server_not_listening_are_not_up() {
        use io::ErrorKind as K;
        for kind in [
            K::ConnectionRefused,
            K::ConnectionReset,
            K::ConnectionAborted,
            K::NotFound,
            K::TimedOut,
        ] {
            assert!(not_listening(kind), "{kind:?}");
        }
        for kind in [K::PermissionDenied, K::InvalidInput, K::Other, K::AddrInUse] {
            assert!(!not_listening(kind), "{kind:?}");
        }
    }

    /// A Unix socket path that does not exist, the state of a host whose Postgres has not
    /// made its socket yet, comes out of the real client as not up.
    #[test]
    fn a_missing_socket_is_not_up() {
        let dir = TestDir::under(std::env::temp_dir(), "kbf-state-not-up-missing");
        let url = format!(
            "postgres:///kbf?host={}",
            dir.path().join("absent").display()
        );
        assert!(open(&url).is_not_up());
    }

    /// A socket file nobody listens on, left by a Postgres that stopped, comes out of the
    /// real client as not up (connection refused).
    #[test]
    fn a_socket_nobody_listens_on_is_not_up() {
        let dir = TestDir::under(std::env::temp_dir(), "kbf-state-not-up-closed");
        // The socket file stays when the listener is dropped.
        drop(UnixListener::bind(dir.path().join(".s.PGSQL.5432")).expect("a socket"));
        let url = format!("postgres:///kbf?host={}", dir.path().display());
        let got = open(&url);
        assert!(got.is_not_up(), "{got:?}");
    }
}
