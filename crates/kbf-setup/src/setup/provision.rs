//! The state database's role and database, and the check that the server admits the logins
//! kbf makes (`setup`, flag `--create-local-database`; docs/deploy/kbf-installer-setup.md,
//! "The state database").
//!
//! With the flag, or on a mode that keeps its state in Postgres when `--database` is not given
//! (then the URL is the local default), on a host whose Postgres runs locally, `setup` asks the
//! server as its superuser and makes what is missing: a login role with no password and no
//! privilege, and a database owned by it. The superuser is reached the way Postgres means it to
//! be on the host: `psql` run as the operating-system user `postgres` over the Unix socket, peer
//! authentication, no password anywhere. `setup` alters nothing that exists: a role that
//! cannot log in or holds a privilege (superuser, CREATEROLE, CREATEDB, REPLICATION,
//! BYPASSRLS), and a database another role owns, are refused, and a rerun finds both there and
//! makes nothing. A host without `psql` or a running server is refused with the command that
//! installs one. It never edits `pg_hba.conf` or `pg_ident.conf`; it prints the lines the
//! logins need ([`access_lines`]), tries the logins and, when the server refuses one, says
//! which rule is missing.
//!
//! Every message, every printed step and every refusal is free of the URL, the role, the
//! database and the socket directory (but for [`access_lines`], which is for the operator to
//! copy): they travel to `psql` in its environment and in `\set` lines on its standard input,
//! neither of which a step prints, and the tool's own words (which quote them) are dropped.

use std::io::Write;

use kbf_host::{Cmd, Entry, Exec, HostPath, RunUser, Step};
use kbf_state::postgres::Unreached;

use super::SetupError;
use super::os::Os;
use super::plan::database_user_name;
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
/// Where a distribution says which family it is of, and the fallback (os-release(5)); on
/// Debian, Ubuntu and Fedora the first is a link to the second.
const OS_RELEASE: [&str; 2] = ["/etc/os-release", "/usr/lib/os-release"];
/// What installs a Postgres server and client on Debian and Ubuntu.
const INSTALL_APT: &str = "apt install postgresql";
/// What installs them on Fedora and its relatives, where the data directory is made by hand.
const INSTALL_DNF: &str = "dnf install postgresql-server, then postgresql-setup --initdb and systemctl enable --now postgresql";
/// The `ID` and `ID_LIKE` words of the distributions that install with `apt`.
const DEBIAN: [&str; 2] = ["debian", "ubuntu"];
/// The `ID` and `ID_LIKE` words of the distributions that install with `dnf`.
const FEDORA: [&str; 3] = ["fedora", "rhel", "centos"];
/// Both, for a host that says nothing of its family.
const INSTALL_EITHER: &str = "apt install postgresql on Debian or Ubuntu; dnf install postgresql-server, then postgresql-setup --initdb and systemctl enable --now postgresql on Fedora";

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

const ROLE_STATE: &str = "SELECT rolcanlogin, rolsuper, rolcreaterole, rolcreatedb, rolreplication, rolbypassrls FROM pg_roles WHERE rolname = :'role';";
const DATABASE_STATE: &str =
    "SELECT pg_get_userbyid(datdba) = :'role' FROM pg_database WHERE datname = :'db';";
const CREATE_ROLE: &str =
    "CREATE ROLE :\"role\" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;";
const CREATE_DATABASE: &str = "CREATE DATABASE :\"db\" OWNER :\"role\";";

fn needs_operator(text: &str) -> SetupError {
    SetupError::NeedsOperator(text.to_owned())
}

/// The words of the `ID` and `ID_LIKE` lines of an os-release file: the families a distribution
/// belongs to.
fn families(release: &str) -> Vec<&str> {
    release
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| matches!(*key, "ID" | "ID_LIKE"))
        .flat_map(|(_, value)| value.trim_matches('"').split_whitespace())
        .collect()
}

/// What installs a Postgres server and client on this host, from the family the first file of
/// [`OS_RELEASE`] that is a file names (a link is not followed, so the fallback is read when the
/// first is one); both commands when neither is a file, or the family is not known.
fn install_command(exec: &dyn Exec) -> &'static str {
    let found = OS_RELEASE.iter().find_map(|path| {
        let entry = HostPath::new(path)
            .ok()
            .and_then(|path| exec.inspect(&path).ok().flatten());
        match entry {
            Some(Entry::File { bytes, .. }) => Some(bytes),
            _ => None,
        }
    });
    let Some(bytes) = found else {
        return INSTALL_EITHER;
    };
    let release = String::from_utf8_lossy(&bytes);
    let family = families(&release);
    let is_of = |names: &[&str]| family.iter().any(|f| names.contains(f));
    if is_of(&DEBIAN) {
        INSTALL_APT
    } else if is_of(&FEDORA) {
        INSTALL_DNF
    } else {
        INSTALL_EITHER
    }
}

/// What the server must say to admit the logins that follow, for the operator to read: the
/// `pg_hba.conf` rule for the role on the database's local socket, with the map that lets root
/// (setup) and the service account log in as the role, and the two lines of that map. Setup
/// edits neither file. These lines name the role and the database: they are for the operator to
/// copy, and each one names one of the two files.
pub fn access_lines(local: &LocalDatabase, mode: Mode) -> [String; 5] {
    let (db, role, service) = (&local.name, &local.role, database_user_name(mode));
    [
        "note: the server must admit setup (as root) and the service, both as the role, over its Unix socket. Setup never edits pg_hba.conf or pg_ident.conf; if no rule of the server's admits them, add these:".to_owned(),
        format!("  pg_hba.conf, above the first `local` line: local  {db}  {role}  peer map=kbf"),
        format!("  pg_ident.conf: kbf  root  {role}"),
        format!("  pg_ident.conf: kbf  {service}  {role}"),
        "  after editing pg_hba.conf or pg_ident.conf, reload the server (not restart); setup tries the logins next and, if the server refuses one, says which rule is missing".to_owned(),
    ]
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
            "could not run {PSQL} ({}): install the Postgres client and server ({}), or create the role and database by hand and pass --database without --create-local-database ({DOCS})",
            e.kind(),
            install_command(exec)
        ))
    })?;
    if !out.status.success() {
        return Err(SetupError::NeedsOperator(format!(
            "{PSQL} could not ask the local Postgres server as its `{SUPERUSER}` user over the Unix socket of the database URL (exit status {}): start the server, install it if it is not there ({}), check the socket directory, or create the role and database by hand and pass --database without --create-local-database ({DOCS})",
            out.status
                .code()
                .map_or_else(|| "none".to_owned(), |c| c.to_string()),
            install_command(exec)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// What the server holds of the role.
enum Role {
    Missing,
    /// Can log in and holds no privilege.
    Plain,
    /// Anything else: the first reason, as the refusal says it.
    Other(&'static str),
}

/// The columns of [`ROLE_STATE`] after `rolcanlogin`, each a privilege a plain role lacks.
const PRIVILEGES: [&str; 5] = [
    "is a superuser",
    "has CREATEROLE",
    "has CREATEDB",
    "has REPLICATION",
    "has BYPASSRLS",
];

fn role_state(answer: &str) -> Result<Role, SetupError> {
    if answer.is_empty() {
        return Ok(Role::Missing);
    }
    let columns: Vec<Option<bool>> = answer
        .split('|')
        .map(|c| match c {
            "t" => Some(true),
            "f" => Some(false),
            _ => None,
        })
        .collect();
    let [Some(login), privileges @ ..] = columns.as_slice() else {
        return Err(unknown_role());
    };
    if privileges.len() != PRIVILEGES.len() || privileges.iter().any(Option::is_none) {
        return Err(unknown_role());
    }
    if !login {
        return Ok(Role::Other("cannot log in"));
    }
    Ok(privileges
        .iter()
        .zip(PRIVILEGES)
        .find(|(held, _)| **held == Some(true))
        .map_or(Role::Plain, |(_, why)| Role::Other(why)))
}

fn unknown_role() -> SetupError {
    needs_operator(
        "the local Postgres server answered a question about the role in a way setup does not know: update kbf-installer, or create the role and database by hand",
    )
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
    if let Role::Other(why) = role {
        return Err(needs_operator(&format!(
            "the role --database names exists but is not a plain login role (it {why}) and setup alters no role: name another role, or fix this one by hand"
        )));
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
    /// The role and database were there: go on to log in and make the schema.
    Schema,
    /// A dry run that would create them: there is nothing to log in to yet.
    Pending,
    /// They were missing and now are made: go on to log in and make the schema.
    Created,
}

/// Makes the role and database `local` names where missing (prints the steps, and with `apply`
/// carries them out). [`Next::Schema`] means both were there and nothing was done.
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
            "making the state database (--create-local-database, and the default of a host that gives no --database) is for a Linux host: it runs psql as the `postgres` account the distribution's package makes".to_owned(),
        ));
    }
    let Some(superuser) = os.user(SUPERUSER)? else {
        return Err(SetupError::NeedsOperator(format!(
            "setup makes the state database with psql run as the operating-system user `{SUPERUSER}`, and this host has none: install the Postgres server ({}), or create the role and database by hand and pass --database without --create-local-database ({DOCS})",
            install_command(&*exec)
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
    Ok(match (steps.is_empty(), apply) {
        (true, _) => Next::Schema,
        (false, true) => Next::Created,
        (false, false) => Next::Pending,
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
    let service = database_user_name(mode);
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
    let service = database_user_name(mode);
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
