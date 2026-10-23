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
    err.code()
        .map_or(Unreached::NoServer, |code| classify_code(code.code()))
}

fn classify_code(code: &str) -> Unreached {
    match code {
        // Class 28 is invalid authorization (no role, no rule, wrong password); 42501 is a
        // role without CONNECT on the database.
        c if c.starts_with("28") || c == "42501" => Unreached::Login,
        "3D000" => Unreached::NoDatabase,
        other => Unreached::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Unreached, classify_code, reach};
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

    /// Catches an SQLSTATE sent to the wrong recipe: authorization failures (class 28) and a
    /// missing CONNECT privilege are a login refusal, a missing database is told apart, and
    /// any other code is passed on as the code alone.
    ///
    /// Catches: the 42501 operand dropped; `3D000` mapped to `Login`.
    #[test]
    fn each_sqlstate_is_told_to_the_recipe_that_fits_it() {
        for code in ["28000", "28P01", "42501"] {
            assert_eq!(classify_code(code), Unreached::Login, "{code}");
        }
        assert_eq!(classify_code("3D000"), Unreached::NoDatabase);
        assert_eq!(classify_code("08006"), Unreached::Other("08006".to_owned()));
    }
}
