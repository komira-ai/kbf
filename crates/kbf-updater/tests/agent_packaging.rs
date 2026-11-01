//! The units and jobs of the host agent (`packaging/host-agent`) against the programs they start:
//! every command line parses as the program parses it, the two ends agree on the socket and on the
//! account that may connect to it, and the agent is not root.

use std::path::{Path, PathBuf};

use kbf_host_install::contract::local::{FrontendConfig, ServeConfig};
use kbf_updater::agent::enroll::parse_flags as parse_enroll;
use kbf_updater::agent::flags::{DEFAULT_HELPER_SOCKET, parse_flags};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/host-agent")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(dir().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The value of `key=` in a unit file's (single) line for it.
fn unit_value(text: &str, key: &str) -> String {
    let found: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("{key}=")))
        .collect();
    assert_eq!(found.len(), 1, "{key} appears once: {found:?}");
    found[0].to_owned()
}

/// Whether a unit names no account (no `User=` line), so systemd runs it as root by default. A
/// unit that names one, even `User=root`, loses CAP_SETUID under its system call filter (the
/// helper's unit says why), and the helper could not drop to another account to run a command.
fn names_no_user(text: &str) -> bool {
    !text.lines().any(|l| l.trim_start().starts_with("User="))
}

/// The strings of the `ProgramArguments` array of a property list.
fn program_arguments(plist: &str) -> Vec<String> {
    let start = plist
        .find("<key>ProgramArguments</key>")
        .expect("ProgramArguments");
    let array = &plist[start..];
    let begin = array.find("<array>").expect("an array") + "<array>".len();
    let end = array.find("</array>").expect("the end of the array");
    array[begin..end]
        .split("<string>")
        .skip(1)
        .map(|s| s.split("</string>").next().expect("closed").to_owned())
        .collect()
}

/// The string after `<key>name</key>`.
fn plist_value(plist: &str, key: &str) -> String {
    let at = plist
        .find(&format!("<key>{key}</key>"))
        .unwrap_or_else(|| panic!("{key}"));
    let rest = &plist[at..];
    let open = rest.find("<string>").expect("a string") + "<string>".len();
    rest[open..]
        .split("</string>")
        .next()
        .expect("closed")
        .to_owned()
}

fn words(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

/// The agent's unit is the agent's command line: it parses with the flags the program has, runs
/// as an account that is not root, and reaches the helper at the socket the helper's unit binds;
/// the helper's unit is root's, lets exactly that account connect, and restarts a unit.
#[test]
fn the_linux_units_agree_with_the_programs_and_with_each_other() {
    let agent = read("linux/kbf-updater-agent.service");
    let line = words(&unit_value(&agent, "ExecStart"));
    assert_eq!(line[1], "agent");
    let config = parse_flags(&line[2..]).expect("the agent's flags");
    assert_eq!(
        config.node_api, None,
        "the shipped unit carries no operator token"
    );
    let exec = unit_value(&agent, "ExecStart");
    for word in ["token", "operator", "proxy"] {
        assert!(!exec.contains(word), "{word}: {exec}");
    }
    // The certificate is the enrollment's, in the unit's own state directory; the frontend is
    // where the node's drain is made (its worker listener) and the enroll listener is where the
    // certificate is renewed.
    let state = config
        .state_dir
        .expect("the unit names the enrollment's directory");
    assert!(config.frontend.is_some());
    assert_ne!(config.frontend, config.enroll_frontend);
    assert_eq!(
        state,
        Path::new("/var/lib").join(unit_value(&agent, "StateDirectory"))
    );
    assert_eq!(unit_value(&agent, "StateDirectoryMode"), "0700");
    // The marks of the agent's cordons go where the unit can write, under ProtectSystem=strict.
    assert_eq!(config.cordon_dir.as_deref(), Some(state.as_path()));
    assert_eq!(config.files.cert, state.join("pki/updater.pem"));
    assert_eq!(config.files.key, state.join("pki/updater.key"));

    // The enrollment command of the README is the program's, for this unit's host: the same
    // enroll listener and the same directory, with the token in a file inside it.
    let example = read("enroll-example.txt");
    let enroll = words(example.trim());
    assert_eq!(enroll[1], "enroll");
    let flags = parse_enroll(&enroll[2..]).expect("the enrollment's flags");
    assert_eq!(Some(flags.frontend), config.enroll_frontend);
    assert_eq!(flags.state_dir, state);
    assert!(flags.token_file.starts_with(&state));
    assert_eq!(config.helper_socket, Path::new(DEFAULT_HELPER_SOCKET));
    let user = unit_value(&agent, "User");
    assert_ne!(user, "root");
    assert!(unit_value(&agent, "After").contains("kbf-host-local.service"));

    let helper = read("linux/kbf-host-local.service");
    let line = words(&unit_value(&helper, "ExecStart"));
    assert_eq!(line[1], "--serve");
    let serve = ServeConfig::parse(&line[2..]).expect("the helper's flags");
    assert_eq!(serve.socket, Path::new(DEFAULT_HELPER_SOCKET));
    assert_eq!(serve.user, user, "only the agent's account may connect");
    assert!(
        names_no_user(&helper),
        "the helper runs as root by default, with no User="
    );
    assert_eq!(unit_value(&helper, "Restart"), "always");
    assert!(!serve.units.is_empty());
    for unit in &serve.units {
        assert!(unit.as_str().starts_with("kbf-"));
    }
    assert!(DEFAULT_HELPER_SOCKET.starts_with(&format!(
        "/run/{}/",
        unit_value(&helper, "RuntimeDirectory")
    )));
    assert_eq!(unit_value(&helper, "StateDirectoryMode"), "0700");

    let sysusers = read("linux/kbf-updater.conf");
    let defined: Vec<&str> = sysusers
        .lines()
        .filter(|l| l.starts_with("u "))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    assert_eq!(defined, [user.as_str()]);
}

/// The helper's unit for a host whose frontend is a container serves the same socket for the same
/// account, takes no unit to restart, and the example of its configuration is a configuration
/// the program accepts, one that fits the window the unit gives a trial.
#[test]
fn the_frontend_container_unit_and_its_configuration_agree_with_the_program() {
    let agent = read("linux/kbf-updater-agent.service");
    let user = unit_value(&agent, "User");
    let helper = read("linux/kbf-host-local-frontend.service");
    let line = words(&unit_value(&helper, "ExecStart"));
    assert_eq!(line[1], "--serve");
    let serve = ServeConfig::parse(&line[2..]).expect("the helper's flags");
    assert!(serve.frontend_container && serve.units.is_empty());
    assert_eq!(serve.socket, Path::new(DEFAULT_HELPER_SOCKET));
    assert_eq!(serve.user, user, "only the agent's account may connect");
    assert!(
        names_no_user(&helper),
        "the helper runs as root by default, with no User="
    );
    assert_eq!(unit_value(&helper, "Restart"), "always");
    assert_eq!(unit_value(&helper, "StateDirectoryMode"), "0700");
    assert!(unit_value(&helper, "After").contains("docker.service"));

    let config = FrontendConfig::parse(&read("linux/host-install-frontend.toml"))
        .expect("the example configuration");
    assert!(
        config.ready_secs <= serve.window_secs,
        "the readiness wait has to fit the window of a trial"
    );
}

/// The same on a Mac: the agent's job parses, runs as an account that is not root, and reaches the
/// helper at the socket the helper's job binds, for the account the helper lets in.
#[test]
fn the_macos_jobs_agree_with_the_programs_and_with_each_other() {
    let agent = read("macos/com.komira.kbf-updater-agent.plist");
    let args = program_arguments(&agent);
    assert_eq!(args[1], "agent");
    let config = parse_flags(&args[2..]).expect("the agent's flags");
    assert_eq!(
        config.node_api, None,
        "the shipped job carries no operator token"
    );
    for word in ["--token-file", "--operator-api", "proxy"] {
        assert!(!agent.contains(&format!("{word}</string>")), "{word}");
    }
    assert!(config.frontend.is_some());
    assert!(config.enroll_frontend.is_some());
    assert_eq!(config.state_dir, Some(PathBuf::from("/var/db/kbf-updater")));
    let user = plist_value(&agent, "UserName");
    assert_ne!(user, "root");

    let helper = read("macos/com.komira.kbf-host-install-local.plist");
    let args = program_arguments(&helper);
    assert_eq!(args[1], "--serve");
    let serve = ServeConfig::parse(&args[2..]).expect("the helper's flags");
    assert_eq!(config.helper_socket, serve.socket);
    assert_eq!(serve.user, user);
    assert!(
        serve.units.is_empty(),
        "a Mac restarts the daemon's job, not a unit"
    );
    assert_eq!(plist_value(&helper, "UserName"), "root");
    for plist in [&agent, &helper] {
        assert!(plist.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
    }
}

/// The words of the `install` command of `install-declaration.txt` that ends in `destination`.
fn install_line(destination: &str) -> Vec<String> {
    let text = read("linux/install-declaration.txt");
    let found: Vec<Vec<String>> = text
        .lines()
        .map(words)
        .filter(|w| w.last().map(String::as_str) == Some(destination))
        .collect();
    assert_eq!(found.len(), 1, "{destination} is installed once: {text}");
    found[0].clone()
}

/// The value of `-o`, `-g` or `-m` in an `install` command.
fn install_option(line: &[String], option: &str) -> String {
    let at = line
        .iter()
        .position(|w| w == option)
        .unwrap_or_else(|| panic!("{option} in {line:?}"));
    line[at + 1].clone()
}

/// Catches a declaration placed so that someone other than root could change it, which the root
/// part would refuse (and which would let that account grant a hook rights): the install commands
/// the README gives make `/etc/updater.d` and `kbf.toml` root's, with no write bit for the group
/// or others, and install the shipped file under its service's name.
#[test]
fn the_kbf_declaration_is_installed_root_owned_and_closed_to_group_and_others() {
    let directory = install_line("/etc/updater.d");
    assert_eq!(directory[1], "-d");
    let file = install_line("/etc/updater.d/kbf.toml");
    for line in [&directory, &file] {
        assert_eq!(line[0], "install");
        assert_eq!(install_option(line, "-o"), "root", "{line:?}");
        assert_eq!(install_option(line, "-g"), "root", "{line:?}");
        let mode = u32::from_str_radix(&install_option(line, "-m"), 8).expect("an octal mode");
        assert_eq!(
            mode & 0o022,
            0,
            "not writable by the group or others: {line:?}"
        );
    }
    let source = &file[file.len() - 2];
    assert_eq!(source, "updater.d/kbf.toml");
    let declaration = update_rights::Declaration::parse(&read(&format!("linux/{source}")))
        .expect("the shipped declaration");
    assert_eq!(declaration.name, "kbf");
}

/// Catches a kbf declaration that runs the hook with other settings than the agent's, that names
/// a boot entry the kbf units do not need, or that grants a unit the helper does not restart: its
/// `hook_args` are `update-hook` and the hook's flags, which parse as the hook parses them and
/// are the agent unit's channel, frontend and enrollment directory, with the declaration's
/// `state_dir` as the record directory; it runs as root, has no boot entry, and its units are
/// exactly the helper's `--restart-unit`s.
#[test]
fn the_kbf_declaration_runs_the_hook_with_the_agents_own_settings() {
    let declaration = update_rights::Declaration::parse(&read("linux/updater.d/kbf.toml"))
        .expect("the shipped declaration");
    assert_eq!(declaration.binary, "kbf");
    assert_eq!(
        declaration.boot, None,
        "the kbf units have an [Install] section"
    );
    assert_eq!(declaration.hook.user, "root");
    assert_eq!(declaration.hook_args[0], "update-hook");
    let (flags, rest) = kbf_updater::agent::hook::flags::split(&declaration.hook_args[1..])
        .expect("the hook's flags");
    assert!(rest.is_empty(), "nothing after the flags: {rest:?}");
    assert_eq!(flags.record_dir, Path::new(&declaration.state_dir));

    let agent = read("linux/kbf-updater-agent.service");
    let line = words(&unit_value(&agent, "ExecStart"));
    let config = parse_flags(&line[2..]).expect("the agent's flags");
    assert_eq!(flags.channel, config.channel);
    assert_eq!(flags.access.frontend, config.frontend);
    assert_eq!(flags.helper_socket, config.helper_socket);
    let state = config.state_dir.expect("the enrollment's directory");
    assert_eq!(flags.access.files.cert, state.join("pki/updater.pem"));
    assert_eq!(flags.access.node_api, None, "no operator token");

    let helper = read("linux/kbf-host-local.service");
    let line = words(&unit_value(&helper, "ExecStart"));
    let serve = ServeConfig::parse(&line[2..]).expect("the helper's flags");
    let restarted: std::collections::BTreeSet<String> =
        serve.units.iter().map(|u| u.as_str().to_owned()).collect();
    assert_eq!(declaration.hook.units, restarted);
}

/// Catches a root part that the agent could not reach, or that serves its registrations from a
/// directory others may enter: its unit's command line parses as the program parses it, runs as
/// root, makes the agent's socket for the agent's account, and its registration socket lies in the
/// directory the tmpfiles entry makes root's, mode 0750, for the group the sysusers entry makes.
#[test]
fn the_root_part_unit_agrees_with_the_agent_and_its_registration_directory() {
    let unit = read("linux/kbf-updater-root.service");
    let line = words(&unit_value(&unit, "ExecStart"));
    assert_eq!(line[1], "root");
    let flags = kbf_updater::root::flags::parse(&line[2..]).expect("the root part's flags");
    assert!(
        names_no_user(&unit),
        "the root part runs as root, with no User="
    );
    let agent = read("linux/kbf-updater-agent.service");
    assert_eq!(flags.socket_user, unit_value(&agent, "User"));
    assert!(
        flags
            .socket
            .starts_with(format!("/run/{}/", unit_value(&unit, "RuntimeDirectory")))
    );
    assert!(unit_value(&unit, "Before").contains("kbf-updater-agent.service"));
    assert_eq!(
        flags.declarations,
        Path::new(update_rights::DECLARATION_DIR)
    );

    let register = flags.register.expect("the registration socket is served");
    let tmpfiles = read("linux/kbf-updater-root.conf");
    let entry: Vec<Vec<String>> = tmpfiles
        .lines()
        .filter(|l| l.starts_with("d "))
        .map(words)
        .collect();
    assert_eq!(entry.len(), 1, "{tmpfiles}");
    assert_eq!(Path::new(&entry[0][1]), register.socket.parent().unwrap());
    assert_eq!(entry[0][2], "0750");
    assert_eq!(entry[0][3], "root");
    assert_eq!(entry[0][4], register.group);
    let sysusers = read("linux/kbf-updater.conf");
    let groups: Vec<&str> = sysusers
        .lines()
        .filter(|l| l.starts_with("g "))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    assert_eq!(groups, [register.group.as_str()]);
}
