//! The state database's role and database, and the check that the server admits the logins
//! kbf makes (`setup`, flag `--create-local-database`; docs/deploy/kbf-installer-setup.md,
//! "The state database").
//!
//! With the flag, on a host whose Postgres runs locally, `setup` asks the server as its
//! superuser and makes what is missing: a login role with no password and no privilege, and a
//! database owned by it. The superuser is reached the way Postgres means it to be on the
//! host: `psql` run as the operating-system user `postgres` over the Unix socket, peer
//! authentication, no password anywhere. `setup` alters nothing that exists: a role that
//! cannot log in or is a superuser, and a database another role owns, are refused. It never
//! edits `pg_hba.conf` or `pg_ident.conf`; it tries the logins and, when the server refuses
//! one, says which rule is missing.
//!
//! Every message, every printed step and every refusal is free of the URL, the role, the
//! database and the socket directory: they travel to `psql` in its environment and in
//! `\set` lines on its standard input, neither of which a step prints, and the tool's own
//! words (which quote them) are dropped.

use std::io::Write;

use kbf_host::{Cmd, Exec, HostPath, RunUser, Step};
use kbf_state::postgres::Unreached;

use super::SetupError;
use super::os::Os;
use super::plan::service_user_name;
use super::validate::LocalDatabase;
use crate::cli::Mode;
use crate::layout::Platform;

/// The Postgres client, where the distributions put it (a wrapper that picks the server's
/// version on Debian).
pub const PSQL: &str = "/usr/bin/psql";
/// The superuser, and the operating-system account that peer authentication maps to it.
pub const SUPERUSER: &str = "postgres";
/// Where the recipe is written down.
const DOCS: &str = "docs/deploy/kbf-installer-setup.md, \"The state database\"";

/// Every psql here: no startup file, no prompts, bare values, and the first error stops it.
const PSQL_ARGS: [&str; 5] = ["-X", "-q", "-A", "-t", "-v"];

/// How a psql reaches the server: its environment, and the account it runs as.
pub struct Connection {
    env: Vec<(String, String)>,
    run_as: Option<RunUser>,
}

impl Connection {
    /// The superuser over the local socket the URL names, as the operating-system user
    /// `postgres` (peer authentication).
    pub fn local(local: &LocalDatabase, superuser: RunUser) -> Connection {
        Connection {
            env: environment(local, SUPERUSER, "postgres"),
            run_as: Some(superuser),
        }
    }

    /// A connection with the given environment and no change of account: how a test reaches
    /// the CI server, which has no `postgres` account to switch to.
    #[cfg(test)]
    pub fn with_env(env: Vec<(String, String)>) -> Connection {
        Connection { env, run_as: None }
    }

    fn command(&self, sql: &str) -> Result<Cmd, SetupError> {
        psql(&self.env, self.run_as.clone(), sql)
    }
}

/// The environment psql reads for where to go. Not arguments: a step prints its arguments and
/// only the names of its environment.
fn environment(local: &LocalDatabase, user: &str, database: &str) -> Vec<(String, String)> {
    let mut env = vec![
        ("PGHOST".to_owned(), local.socket_dir.clone()),
        ("PGUSER".to_owned(), user.to_owned()),
        ("PGDATABASE".to_owned(), database.to_owned()),
        ("PGCONNECT_TIMEOUT".to_owned(), "10".to_owned()),
    ];
    if let Some(port) = local.port {
        env.push(("PGPORT".to_owned(), port.to_string()));
    }
    env
}

fn psql(env: &[(String, String)], run_as: Option<RunUser>, sql: &str) -> Result<Cmd, SetupError> {
    let mut cmd = Cmd::new(HostPath::new(PSQL)?);
    for arg in PSQL_ARGS {
        cmd = cmd.arg(arg);
    }
    cmd = cmd.arg("ON_ERROR_STOP=1").stdin(sql.as_bytes().to_vec());
    for (key, value) in env {
        cmd = cmd.env(key, value);
    }
    if let Some(user) = run_as {
        cmd = cmd.user(user);
    }
    Ok(cmd)
}

/// The two names, set as psql variables on the script's first lines. The names are plain (the
/// flags' check), so a single-quoted `\set` is exact; the SQL then takes them quoted as a
/// literal (`:'role'`) or an identifier (`:"role"`).
fn script(local: &LocalDatabase, sql: &str) -> String {
    format!(
        "\\set role '{}'\n\\set db '{}'\n{sql}\n",
        local.role, local.name
    )
}

const ROLE_STATE: &str = "SELECT rolcanlogin, rolsuper FROM pg_roles WHERE rolname = :'role';";
const DATABASE_STATE: &str =
    "SELECT pg_get_userbyid(datdba) = :'role' FROM pg_database WHERE datname = :'db';";
const CREATE_ROLE: &str =
    "CREATE ROLE :\"role\" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;";
const CREATE_DATABASE: &str = "CREATE DATABASE :\"db\" OWNER :\"role\";";

fn needs_operator(text: &str) -> SetupError {
    SetupError::NeedsOperator(text.to_owned())
}

/// Runs `sql` and returns its output, or the refusal that says the local server could not be
/// asked. Nothing of the tool's own words is kept.
fn ask(
    exec: &dyn Exec,
    conn: &Connection,
    local: &LocalDatabase,
    sql: &str,
) -> Result<String, SetupError> {
    let out = exec.query(&conn.command(&script(local, sql))?).map_err(|e| {
        SetupError::NeedsOperator(format!(
            "could not run {PSQL} ({}): install the Postgres client, or create the role and database by hand and run setup without --create-local-database ({DOCS})",
            e.kind()
        ))
    })?;
    if !out.status.success() {
        return Err(SetupError::NeedsOperator(format!(
            "{PSQL} could not ask the local Postgres server as its `{SUPERUSER}` user over the Unix socket --database names (exit status {}): start the server, check the socket directory, or create the role and database by hand and run setup without --create-local-database ({DOCS})",
            out.status
                .code()
                .map_or_else(|| "none".to_owned(), |c| c.to_string())
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// What the server holds of the role.
enum Role {
    Missing,
    /// Can log in, is no superuser.
    Plain,
    Other,
}

fn role_state(answer: &str) -> Result<Role, SetupError> {
    match answer {
        "" => Ok(Role::Missing),
        "t|f" => Ok(Role::Plain),
        "f|f" | "t|t" | "f|t" => Ok(Role::Other),
        _ => Err(needs_operator(
            "the local Postgres server answered a question about the role in a way setup does not know: update kbf-installer, or create the role and database by hand",
        )),
    }
}

/// The steps that make the role and the database, read from what the server holds; none when
/// both are there. Reads only: the steps are the caller's to print and apply.
///
/// # Errors
/// [`SetupError::NeedsOperator`]: the server cannot be asked, or holds a role or a database
/// setup will not alter.
pub fn plan(
    exec: &dyn Exec,
    conn: &Connection,
    local: &LocalDatabase,
) -> Result<Vec<(&'static str, Step)>, SetupError> {
    let role = role_state(&ask(exec, conn, local, ROLE_STATE)?)?;
    if matches!(role, Role::Other) {
        return Err(needs_operator(
            "the role --database names exists but is not a plain login role (it cannot log in, or it is a superuser) and setup alters no role: name another role, or fix this one by hand",
        ));
    }
    let owned = match ask(exec, conn, local, DATABASE_STATE)?.as_str() {
        "" => None,
        "t" => Some(true),
        "f" => Some(false),
        _ => {
            return Err(needs_operator(
                "the local Postgres server answered a question about the database in a way setup does not know: update kbf-installer, or create the role and database by hand",
            ));
        }
    };
    if owned == Some(false) {
        return Err(needs_operator(
            "the database --database names exists and another role owns it; setup changes no owner: name another database, or give this one to the role by hand",
        ));
    }
    let mut steps = Vec::new();
    if matches!(role, Role::Missing) {
        steps.push((
            "create the login role",
            Step::Run(conn.command(&script(local, CREATE_ROLE))?),
        ));
    }
    if owned.is_none() {
        steps.push((
            "create the database",
            Step::Run(conn.command(&script(local, CREATE_DATABASE))?),
        ));
    }
    Ok(steps)
}

/// What `setup` does next about the database.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// The role and database are there (or now are): go on to log in and make the schema.
    Schema,
    /// A dry run that would create them: there is nothing to log in to yet.
    Pending,
}

/// Makes the role and database `local` names where missing (prints the steps, and with `apply`
/// carries them out).
///
/// # Errors
/// [`SetupError`]: this is not a Linux host with a `postgres` account, the server cannot be
/// asked, it holds something setup will not alter, or a creation failed.
pub fn provision(
    os: &dyn Os,
    exec: &mut dyn Exec,
    local: &LocalDatabase,
    apply: bool,
    out: &mut dyn Write,
) -> Result<Next, SetupError> {
    if os.platform() != Platform::Linux {
        return Err(SetupError::Unsupported(
            "--create-local-database is for a Linux host: it runs psql as the `postgres` account the distribution's package makes".to_owned(),
        ));
    }
    let Some(superuser) = os.user(SUPERUSER)? else {
        return Err(SetupError::NeedsOperator(format!(
            "--create-local-database runs psql as the operating-system user `{SUPERUSER}`, and this host has none: install the Postgres server (apt install postgresql), or create the role and database by hand and run setup without the flag ({DOCS})"
        )));
    };
    let steps = plan(&*exec, &Connection::local(local, superuser), local)?;
    for (what, step) in &steps {
        if apply {
            writeln!(out, "+ {what}: {step}")?;
            // The tool's own words quote the names: dropped.
            exec.apply(step).map_err(|_| {
                SetupError::NeedsOperator(format!(
                    "{what} failed (the Postgres server's log says why); setup can be run again"
                ))
            })?;
        } else {
            writeln!(out, "DRY-RUN: {what}: {step}")?;
        }
    }
    Ok(if !apply && !steps.is_empty() {
        Next::Pending
    } else {
        Next::Schema
    })
}

/// Whether the account the service runs as can log in to the database: the one login `setup`
/// does not make itself. A host with no such account is the plan's to refuse.
///
/// # Errors
/// [`SetupError::NeedsOperator`] with the recipe when the server refuses the login.
pub fn check_service_login(
    os: &dyn Os,
    exec: &dyn Exec,
    local: &LocalDatabase,
    mode: Mode,
) -> Result<(), SetupError> {
    let service = service_user_name(mode);
    let Some(user) = os.user(service)? else {
        return Ok(());
    };
    let env = environment(local, &local.role, &local.name);
    let cmd = psql(&env, Some(user), "SELECT 1;\n")?;
    match exec.query(&cmd) {
        Ok(out) if out.status.success() => Ok(()),
        _ => Err(unreached(&Unreached::Login, mode)),
    }
}

/// The refusal for a login the server did not admit, saying what to do and naming no value
/// the operator wrote. The recipe's rules are in the docs with their names filled in.
pub fn unreached(why: &Unreached, mode: Mode) -> SetupError {
    let service = service_user_name(mode);
    let text = match why {
        Unreached::NoServer => format!(
            "no Postgres server answered at the Unix socket --database names: install and start one (apt install postgresql; the kbf-installer package only suggests it, because node hosts need none) and run setup again, or correct host= in --database ({DOCS})"
        ),
        Unreached::Login => format!(
            "the Postgres server refused the login that --database names (the role is missing, or pg_hba.conf has no rule that admits this account). Setup never edits pg_hba.conf. Create the role and database with --create-local-database, or by hand, and add these, with your role and database for <role> and <database>: in pg_hba.conf, above the first `local` line, `local  <database>  <role>  peer map=kbf`; in pg_ident.conf, `kbf  root  <role>` and `kbf  {service}  <role>`; then reload the server (not restart) and run setup again ({DOCS})"
        ),
        Unreached::NoDatabase => format!(
            "the Postgres server has no database of the name --database gives: create it, owned by the role, or run setup with --create-local-database ({DOCS})"
        ),
        Unreached::Other(code) => format!(
            "the Postgres server answered the login with an error (SQLSTATE {code}); its log says why ({DOCS})"
        ),
    };
    SetupError::NeedsOperator(text)
}
