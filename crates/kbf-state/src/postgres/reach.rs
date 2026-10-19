//! Whether the database a URL names can be logged in to, told apart by what a person has
//! to do about it. The installer's `setup` uses it to refuse with a recipe instead of the
//! server's own words, which quote the role and the database. Nothing here keeps or prints
//! the URL, a role or a database name.

use super::PostgresConfig;
use super::conn::Conn;

/// Why a login to the database failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unreached {
    /// Nothing answered: no server, no socket at that path, a refused or timed-out
    /// connection, or a URL the client cannot connect with.
    #[error("no Postgres server answered at the address the URL names")]
    NoServer,
    /// The server answered and refused this login: the role does not exist, the role may
    /// not log in, or `pg_hba.conf` has no rule that admits this operating-system user.
    #[error("the server refused the login")]
    Login,
    /// The server answered and has no such database.
    #[error("the server has no such database")]
    NoDatabase,
    /// Any other answer, by its SQLSTATE (a code, never a name).
    #[error("the server answered with an error (SQLSTATE {0})")]
    Other(String),
}

/// Logs in to the database `config` names and drops the connection.
///
/// # Errors
/// [`Unreached`]: what stopped the login.
pub fn reach(config: &PostgresConfig) -> Result<(), Unreached> {
    Conn::connect(&config.pg)
        .map(drop)
        .map_err(|e| classify(&e))
}

fn classify(err: &postgres::Error) -> Unreached {
    let Some(code) = err.code() else {
        return Unreached::NoServer;
    };
    match code.code() {
        // Class 28 is invalid authorization (no role, no rule, wrong password); 42501 is a
        // role without CONNECT on the database.
        c if c.starts_with("28") || c == "42501" => Unreached::Login,
        "3D000" => Unreached::NoDatabase,
        other => Unreached::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Unreached, reach};
    use crate::postgres::PostgresConfig;

    /// Catches a connection failure with no server being reported as anything but
    /// `NoServer` (it would send the operator to pg_hba.conf), and a message that quotes
    /// the socket directory, the role or the database.
    #[test]
    fn a_socket_with_no_server_behind_it_is_no_server_and_names_nothing() {
        let url = "postgres:///secretdb?host=/nonexistent-socket-dir&user=secretrole";
        let config = PostgresConfig::parse(url).expect("parses");
        let err = reach(&config).unwrap_err();
        assert_eq!(err, Unreached::NoServer);
        let text = err.to_string();
        for name in ["secretdb", "secretrole", "nonexistent"] {
            assert!(!text.contains(name), "{text}");
        }
    }

    fn server() -> PostgresConfig {
        let url = std::env::var("KBF_TEST_POSTGRES_URL")
            .expect("KBF_TEST_POSTGRES_URL names a test database");
        PostgresConfig::parse(&url).expect("parses")
    }

    /// Catches a login to a reachable database being reported as a failure.
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn a_login_the_server_admits_is_reached() {
        assert_eq!(reach(&server()), Ok(()));
    }

    /// Catches a missing role being told as anything but `Login` (the recipe for it is the
    /// role and the pg_hba rule), and a missing database as anything but `NoDatabase`.
    /// The text of each names neither.
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn a_missing_role_is_a_login_refusal_and_a_missing_database_is_told_apart() {
        let mut no_role = server();
        no_role.pg.user("kbf_no_such_role_zz");
        let err = reach(&no_role).unwrap_err();
        assert_eq!(err, Unreached::Login);
        assert!(!err.to_string().contains("kbf_no_such_role_zz"), "{err}");

        let mut no_db = server();
        no_db.pg.dbname("kbf_no_such_db_zz");
        let err = reach(&no_db).unwrap_err();
        assert_eq!(err, Unreached::NoDatabase);
        assert!(!err.to_string().contains("kbf_no_such_db_zz"), "{err}");
    }
}
