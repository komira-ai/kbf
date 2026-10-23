//! The default of `frontend` and `all-in-one`: given no `--database`, setup makes the local role
//! and database, and tells the operator what the server must admit. The fake host and the model
//! of a server are `provision_tests.rs`'s. Each test says the behaviour it asserts.

use super::*;

const OS_RELEASE: &str = "/etc/os-release";
const USR_OS_RELEASE: &str = "/usr/lib/os-release";
/// What installs a server and a client on Debian and Ubuntu.
const APT: &str = "apt install postgresql";
/// What installs them on Fedora and its relatives.
const DNF: &str = "dnf install postgresql-server";
/// The rules the logins of the default need, as setup prints them.
const HBA: &str =
    "  pg_hba.conf, above the first `local` line: local  kbf  kbf-server  peer map=kbf";
const IDENT_ROOT: &str = "  pg_ident.conf: kbf  root  kbf-server";
const IDENT_SERVICE: &str = "  pg_ident.conf: kbf  kbf-server  kbf-server";

/// The flags of a host that gives no `--database`.
fn bare(mode: Mode) -> Raw {
    let mut r = raw(mode);
    r.database = None;
    r
}

/// Asserts that the rules the logins need are in `out`, once each.
fn assert_rules_once(out: &str) {
    for line in [HBA, IDENT_ROOT, IDENT_SERVICE] {
        assert_eq!(out.matches(line).count(), 1, "{line}: {out}");
    }
}

/// The scripts of the creations the server was asked for.
fn creates(world: &Shared) -> Vec<String> {
    world
        .borrow()
        .ran
        .iter()
        .filter(|(_, script)| script.contains("CREATE"))
        .map(|(_, script)| script.clone())
        .collect()
}

/// A host whose `psql` is not installed, with this `os-release`. Every other program runs: the
/// account steps that come before the database step must be able to succeed.
fn no_psql(os_release: &str) -> FakeExec {
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, OS_RELEASE, 0o644, ROOT, os_release);
    exec.answer = Box::new(|cmd| {
        if cmd.program.as_str() == provision::PSQL {
            Err(io::Error::from(io::ErrorKind::NotFound))
        } else {
            Ok(reply(0, "", ""))
        }
    });
    exec
}

/// Asserts that a frontend given no `--database` plans and applies the provisioning of the local
/// database: the role and then the database, both run as `postgres` over the local socket, the
/// schema made after them and before `kbf.toml`, and the local default is the URL both files
/// carry.
#[test]
fn a_frontend_with_no_database_gets_the_local_role_database_and_schema() {
    let world = Shared::default();
    let server = Server::fresh();
    let (exec, out) = setup_on(&world, &server, &bare(Mode::Frontend), APPLY);
    let out = out.expect("setup");
    let made = creates(&world);
    assert_eq!(made.len(), 2, "{made:?}");
    assert!(made[0].contains("CREATE ROLE"), "{made:?}");
    assert!(made[1].contains("CREATE DATABASE"), "{made:?}");
    let w = world.borrow();
    let set = "\\set role 'kbf-server'\n\\set db 'kbf'\n";
    for (user, script) in w.ran.iter().filter(|(_, s)| s.contains("CREATE")) {
        assert_eq!(user.as_deref(), Some("postgres"));
        assert!(script.starts_with(set));
    }
    assert!(w.envs[0].contains(&"PGHOST=/run/postgresql".to_owned()));
    assert_eq!(w.role, Some(Row::plain()));
    assert_eq!(w.database, Some(true));
    assert_eq!(
        *server.asked.borrow(),
        [format!("reach {DATABASE}"), format!("setup {DATABASE}")]
    );
    let line = format!("database = \"{DATABASE}\"\n");
    assert!(text(&exec, INSTALLER_TOML).contains(&line));
    assert_eq!(text(&exec, KBF_TOML), FRONTEND_TOML);
    let at = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {out}"))
    };
    let role = at("+ create the login role");
    let database = at("+ create the database");
    let schema = at("database step: created the schema");
    let kbf_toml = at(KBF_TOML);
    assert!(role < database);
    assert!(database < schema);
    assert!(schema < kbf_toml);
}

/// Asserts that an all-in-one host given no `--database` provisions the local database as a
/// frontend does, and that the children's file and `installer.toml` carry the local default.
#[test]
fn an_all_in_one_host_with_no_database_gets_the_local_role_database_and_schema() {
    let world = Shared::default();
    let server = Server::fresh();
    let (exec, out) = setup_on(&world, &server, &bare(Mode::AllInOne), APPLY);
    out.expect("setup");
    let made = creates(&world);
    assert_eq!(made.len(), 2, "{made:?}");
    assert_eq!(
        *server.asked.borrow(),
        [format!("reach {DATABASE}"), format!("setup {DATABASE}")]
    );
    let store = format!("state-store = \"{DATABASE}\"\n");
    assert!(text(&exec, AIO_FRONTEND_TOML).contains(&store));
    let database = format!("database = \"{DATABASE}\"\n");
    assert!(text(&exec, INSTALLER_TOML).contains(&database));
}

/// Asserts that a daemon setup does not provision: psql is never run, no schema is asked for,
/// and nothing is said of the server's rules.
#[test]
fn a_daemon_setup_does_not_provision_a_database() {
    let world = Shared::default();
    let server = Server::fresh();
    let (_, out) = setup_on(&world, &server, &raw(Mode::Daemon), APPLY);
    let out = out.expect("setup");
    assert!(world.borrow().ran.is_empty());
    assert!(server.asked.borrow().is_empty());
    assert!(!out.contains("pg_hba.conf"), "{out}");
}

/// Asserts that an explicit `--database` skips the provisioning, as it did before the default:
/// psql is never run, so no role or database is made and no rule is printed, and the schema is
/// still made at the URL given.
#[test]
fn an_explicit_database_skips_the_provisioning() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let world = Shared::default();
        let server = Server::fresh();
        let (_, out) = setup_on(&world, &server, &raw(mode), APPLY);
        let out = out.expect("setup");
        assert!(world.borrow().ran.is_empty());
        assert!(!out.contains("pg_hba.conf"), "{out}");
        assert!(out.contains("database step: created the schema"), "{out}");
    }
}

/// Asserts that a rerun of the default changes nothing: the host is as the first run left it,
/// no role or database is made again, the output says there is nothing to do, and it still has
/// the rules the logins need, with the names filled in, for an operator who lost the first
/// run's output.
#[test]
fn a_rerun_of_the_default_changes_nothing() {
    let world = Shared::default();
    let mut exec = exec_on(&world);
    let first = go(&linux(), &mut exec, &bare(Mode::Frontend), APPLY);
    first.expect("first run");
    assert_eq!(creates(&world).len(), 2);
    let steps = exec.log.len();
    let mut os = settled(&["kbf-frontend.service"]);
    os.users.push(user("postgres", 120));
    let current = Server {
        found: Some(3),
        ..Server::fresh()
    };
    let again = go_on(&os, &mut exec, &current, &bare(Mode::Frontend), APPLY);
    let out = again.expect("a rerun");
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
    assert_eq!(creates(&world).len(), 2);
    assert!(out.contains("\nnothing to do\n"), "{out}");
    assert_rules_once(&out);
}

/// Asserts that an existing role and database are never altered: both there and plain, nothing
/// is created; only the database missing, only it is created; a role with a privilege or a
/// database another role owns is refused, nothing is created or altered, and `kbf.toml` waits.
#[test]
fn an_existing_role_and_database_are_left_as_they_are() {
    let world = Shared::default();
    world.borrow_mut().role = Some(Row::plain());
    world.borrow_mut().database = Some(true);
    let (_, out) = setup_on(&world, &Server::fresh(), &bare(Mode::Frontend), APPLY);
    let out = out.expect("setup");
    assert!(creates(&world).is_empty());
    assert!(!out.contains("create the"), "{out}");
    assert_rules_once(&out);
    assert!(out.contains("database step: created the schema"), "{out}");

    let world = Shared::default();
    world.borrow_mut().role = Some(Row::plain());
    let (_, out) = setup_on(&world, &Server::fresh(), &bare(Mode::Frontend), APPLY);
    out.expect("setup");
    let made = creates(&world);
    assert_eq!(made.len(), 1, "{made:?}");
    assert!(made[0].contains("CREATE DATABASE"), "{made:?}");

    let privileged = Row {
        superuser: true,
        ..Row::plain()
    };
    for (role, owned, why) in [
        (Some(privileged), None, "not a plain login role"),
        (Some(Row::plain()), Some(false), "another role owns it"),
    ] {
        let world = Shared::default();
        world.borrow_mut().role = role;
        world.borrow_mut().database = owned;
        let (exec, out) = setup_on(&world, &Server::fresh(), &bare(Mode::Frontend), APPLY);
        let said = refusal(out);
        assert!(said.contains(why), "{said}");
        let w = world.borrow();
        assert!(w.ran.iter().all(|(_, s)| !s.contains("CREATE")));
        assert!(w.ran.iter().all(|(_, s)| !s.contains("ALTER")));
        assert_eq!(w.role, role);
        assert_eq!(w.database, owned);
        assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
    }
}

/// Asserts that a host without psql is refused with the command that installs Postgres for its
/// distribution (apt for Debian and Ubuntu, dnf and the initialisation for Fedora and its
/// relatives, both when `os-release` is missing, is not a file or names no known family), with
/// the way to do it by hand, in a dry run as in an apply, and that no `kbf.toml` is written.
#[test]
fn a_host_without_psql_is_refused_with_the_install_command_of_its_family() {
    let debian = "# a comment\n\nNAME=\"Debian GNU/Linux\"\nID=debian\n";
    let ubuntu = "NAME=Ubuntu\nID=ubuntu\nID_LIKE=debian\n";
    let rocky = "NAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n";
    for (mut exec, wanted, unwanted) in [
        (no_psql(debian), APT, DNF),
        (no_psql(ubuntu), APT, DNF),
        (no_psql(rocky), DNF, APT),
        (no_psql("ID=fedora\n"), DNF, APT),
    ] {
        let out = go(&linux(), &mut exec, &bare(Mode::Frontend), APPLY);
        let said = refusal(out);
        assert!(said.contains(wanted), "{said}");
        assert!(!said.contains(unwanted), "{said}");
        let initdb = said.contains("--initdb");
        assert_eq!(initdb, wanted == DNF, "{said}");
        let by_hand = "create the role and database by hand and pass --database";
        assert!(said.contains(by_hand), "{said}");
        assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
    }

    let mut a_directory = no_psql("");
    a_directory.entries.insert(
        hp(OS_RELEASE),
        Entry::Dir {
            perms: Perms::new(0o755),
            owner: ROOT,
        },
    );
    let mut missing = no_psql("");
    missing.entries.remove(&hp(OS_RELEASE));
    for mut exec in [no_psql("ID=arch\n"), a_directory, missing] {
        let out = go(&linux(), &mut exec, &bare(Mode::Frontend), APPLY);
        let said = refusal(out);
        assert!(said.contains(APT), "{said}");
        assert!(said.contains(DNF), "{said}");
    }

    let mut exec = no_psql(ubuntu);
    let out = go(&linux(), &mut exec, &bare(Mode::AllInOne), DRY);
    assert!(refusal(out).contains(APT));
    assert!(exec.log.is_empty());
}

/// Asserts that `/etc/os-release` being a link, as it is on Debian, Ubuntu and Fedora, does not
/// hide the family: the file it links to is read as `/usr/lib/os-release`, and with no such file
/// behind the link both commands are given.
#[test]
fn a_linked_os_release_is_read_through_its_fallback() {
    let link = Entry::Link {
        target: "../usr/lib/os-release".into(),
    };
    for (family, wanted, unwanted) in [("ID=debian\n", APT, DNF), ("ID=fedora\n", DNF, APT)] {
        let mut exec = no_psql("");
        exec.entries.insert(hp(OS_RELEASE), link.clone());
        put(&mut exec, USR_OS_RELEASE, 0o644, ROOT, family);
        let said = refusal(go(&linux(), &mut exec, &bare(Mode::Frontend), APPLY));
        assert!(said.contains(wanted), "{said}");
        assert!(!said.contains(unwanted), "{said}");
    }

    let mut exec = no_psql("");
    exec.entries.insert(hp(OS_RELEASE), link);
    let said = refusal(go(&linux(), &mut exec, &bare(Mode::Frontend), APPLY));
    assert!(said.contains(APT), "{said}");
    assert!(said.contains(DNF), "{said}");
}

/// Asserts that a stopped server, and a host with no `postgres` account, are refused with the
/// install command of the distribution and the way to start the server, and that neither
/// `kbf.toml` nor the frontend's file is written.
#[test]
fn a_stopped_server_and_a_missing_server_are_refused_with_the_install_command() {
    let world = Shared::default();
    world.borrow_mut().broken = Some(2);
    let mut exec = exec_on(&world);
    put(&mut exec, OS_RELEASE, 0o644, ROOT, "ID=fedora\n");
    let out = go(&linux(), &mut exec, &bare(Mode::AllInOne), APPLY);
    let said = refusal(out);
    for wanted in ["exit status 2", "start the server", DNF, "--initdb"] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
    assert!(!exec.entries.contains_key(&hp(AIO_FRONTEND_TOML)));
    assert!(!exec.entries.contains_key(&hp(KBF_TOML)));

    let mut exec = exec_on(&world);
    put(&mut exec, OS_RELEASE, 0o644, ROOT, "ID=ubuntu\n");
    let out = go(&FakeOs::linux(), &mut exec, &bare(Mode::Frontend), APPLY);
    let said = refusal(out);
    let wanted = "has none: install the Postgres server (apt install postgresql)";
    assert!(said.contains(wanted), "{said}");
    assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
}

/// Asserts that a frontend given no `--database` plans the provisioning in a dry run (the role
/// and the database as `DRY-RUN` steps, nothing created) and does it in an apply, and that both
/// print the rules the server needs for the logins (the `pg_hba.conf` line of the role on the
/// database's local socket with its map, and the two `pg_ident.conf` lines that let root and the
/// service log in as the role), once each, while setup writes neither file; and that a role and
/// database given with `--create-local-database` are the names printed.
#[test]
fn the_rules_the_logins_need_are_printed_in_a_dry_run_and_an_apply() {
    for options in [DRY, APPLY] {
        let world = Shared::default();
        let (exec, out) = setup_on(&world, &Server::fresh(), &bare(Mode::Frontend), options);
        let out = out.expect("setup");
        let planned = out.contains("DRY-RUN: create the login role");
        assert_eq!(planned, !options.apply, "{out}");
        let planned = out.contains("DRY-RUN: create the database");
        assert_eq!(planned, !options.apply, "{out}");
        assert_eq!(creates(&world).is_empty(), !options.apply, "{out}");
        assert_rules_once(&out);
        assert!(out.contains("Setup never edits pg_hba.conf"), "{out}");
        let steps: Vec<String> = exec.log.iter().map(ToString::to_string).collect();
        assert!(steps.iter().all(|s| !s.contains("pg_hba")));
        assert!(steps.iter().all(|s| !s.contains("pg_ident")));
    }

    let (_, out) = setup_on(&Shared::default(), &Server::fresh(), &flags(), DRY);
    let out = out.expect("dry run");
    let hba = "local  secretdb  secretrole  peer map=kbf";
    assert!(out.contains(hba), "{out}");
    let ident = "kbf  kbf-server  secretrole";
    assert!(out.contains(ident), "{out}");
}

/// Asserts that the default names the database `kbf` on the local socket, reached as the
/// account the frontend runs as, for both modes and whether or not `--create-local-database` is
/// also given; and that a node mode has no database, defaulted or not.
#[test]
fn the_default_is_the_kbf_database_on_the_local_socket() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        for flag in [false, true] {
            let mut r = bare(mode);
            r.create_local_database = flag;
            let valid = r.validate(Platform::Linux).expect("valid");
            assert!(valid.database_defaulted);
            let local = valid.local_database.expect("provisioned");
            assert_eq!(
                (
                    local.role.as_str(),
                    local.name.as_str(),
                    local.socket_dir.as_str(),
                    local.port
                ),
                ("kbf-server", "kbf", "/run/postgresql", None)
            );
        }
    }
    let node = bare(Mode::Daemon);
    let valid = node.validate(Platform::Linux).expect("valid");
    assert!(!valid.database_defaulted);
    assert!(valid.database.is_none());
    assert!(valid.local_database.is_none());
}

/// Asserts that a database the `installer.toml` already names is not replaced by the default
/// when `--database` is left out: setup is refused (exit 5, `--database` named, no value
/// printed) before anything is written, and the file and the server are as they were.
#[test]
fn a_database_the_installer_toml_names_is_not_replaced_by_the_default() {
    let other = "postgres:///other?host=/run/postgresql&user=kbf-server";
    let mut first = raw(Mode::Frontend);
    first.database = Some(other.to_owned());
    let world = Shared::default();
    let mut exec = exec_on(&world);
    go(&linux(), &mut exec, &first, APPLY).expect("the first run");
    let before = text(&exec, INSTALLER_TOML);
    assert!(before.contains(&format!("database = \"{other}\"\n")));
    let steps = exec.log.len();
    let out = go(&linux(), &mut exec, &bare(Mode::Frontend), APPLY);
    let err = out.expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let said = err.to_string();
    assert!(said.contains("give --database"), "{said}");
    assert!(!said.contains("other"), "{said}");
    assert_eq!(exec.log.len(), steps);
    assert_eq!(text(&exec, INSTALLER_TOML), before);
    assert!(world.borrow().ran.is_empty());
}

/// Asserts that a Mac is refused for a frontend with no `--database` before anything is asked of
/// a server: the default does not reach a platform that has no `postgres` account to switch to.
#[test]
fn a_mac_frontend_with_no_database_is_refused_before_a_server_is_asked() {
    let world = Shared::default();
    let mut exec = released(host(), Platform::MacOs);
    let w = Rc::clone(&world);
    exec.answer = Box::new(move |cmd| psql_answer(&w, cmd));
    let out = go(&FakeOs::mac(), &mut exec, &bare(Mode::Frontend), APPLY);
    let err = out.expect_err("refused");
    assert_eq!(err.exit(), Exit::Usage, "{err}");
    assert!(err.to_string().contains("a Mac is a node only"), "{err}");
    assert!(world.borrow().ran.is_empty());
}
