//! Tests of the move of an all-in-one host of the earlier layout (a `kbf.toml` that says `mode =
//! "all-in-one"`) to the proxy layout: the split into the sibling's, the children's and the
//! installer's files, the original kept, what flags override, what has no place, the operator
//! token and the commands left to the operator. The helpers are `tests.rs`'s. Each test says the
//! behaviour it asserts.

use super::*;
use crate::setup::aio_migrate;

/// The earlier layout's `kbf.toml`, and the three files its split gives (the files of a fresh
/// all-in-one host: the earlier golden was written with the values of the fresh host's flags).
const EARLIER: &str = include_str!("../../tests/golden/kbf_toml/all-in-one.toml");
const SIBLING_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/all-in-one-sibling.toml");
const FRONTEND_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/all-in-one-frontend.toml");
const INSTALLER_GOLDEN: &str = include_str!("../../tests/golden/all_in_one/installer.toml");
/// Where setup keeps the original, and the directory it makes for it.
const COPY: &str = "/var/lib/kbf-installer/migration-units/kbf.toml.all-in-one";
const COPY_DIR: &str = "/var/lib/kbf-installer/migration-units";
/// The earlier setup's operator token (it belonged to `kbf`), and one at another path.
const OLD_TOKEN: &str = "old-token-bytes\n";
const OTHER_TOKEN: &str = "/etc/kbf/other-token";
const OLD_UNIT: &str = "kbf-all-in-one.service";
/// The last command of the move, in two parts: the sibling first, then the proxy.
const FOURTH: &str = "migration 4/4: systemctl start kbf-storage-and-daemon.service, then";
const THEN_PROXY: &str = "systemctl start kbf-proxy.socket kbf-proxy.service";
/// Root with the group of `kbf` and of `kbf-server` on the fake host: who owns the two files.
const GROUP_KBF: Owner = Owner { uid: 0, gid: 990 };
const GROUP_SERVER: Owner = Owner { uid: 0, gid: 989 };
const UNITS: [&str; 3] = [
    "kbf-proxy.socket",
    "kbf-proxy.service",
    "kbf-storage-and-daemon.service",
];
const DRY: Options = Options {
    apply: false,
    no_start: false,
};
/// The lines of `EARLIER` that the proxy layout has no place for: the listeners and the worker
/// listener's certificate and key.
const UNPLACED_LINES: [&str; 5] = [
    "listen = \"0.0.0.0:8980\"\n",
    "worker-listen = \"0.0.0.0:8981\"\n",
    "api-listen = \"127.0.0.1:8982\"\n",
    "worker-tls-cert = \"/etc/kbf/worker.pem\"\n",
    "worker-tls-key = \"/etc/kbf/worker.key\"\n",
];

fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

/// A host of the earlier layout: a release installed, the files the flags name, `kbf_toml` as the
/// earlier `kbf.toml` and the operator token the earlier setup made, owned by `kbf`.
fn earlier_host(kbf_toml: &str) -> FakeExec {
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, KBF_TOML, 0o640, GROUP_KBF, kbf_toml);
    put(&mut exec, API_TOKEN, 0o600, KBF, OLD_TOKEN);
    exec
}

/// The flags of a run that gives none of what the earlier file has: `--proxy`, the release and
/// nothing else.
fn minimal() -> Raw {
    let mut flags = raw(Mode::AllInOne);
    flags.kbf = Default::default();
    flags.database = None;
    flags
}

/// The earlier file with `[tls] api-token-file` naming `path`.
fn token_named(path: &str) -> String {
    let key = "api-token-file = \"/etc/kbf/api-token\"";
    EARLIER.replace(key, &format!("api-token-file = \"{path}\""))
}

fn dir_meta(exec: &FakeExec, path: &str) -> (u32, Owner) {
    match exec.entries.get(&hp(path)) {
        Some(Entry::Dir { perms, owner }) => (perms.bits(), *owner),
        other => panic!("{path}: {other:?}"),
    }
}

/// Catches the move missing a setting, a file in the wrong hands, or the original not kept first:
/// a run that gives no setting flag still ends with the three files of a fresh host (byte for
/// byte the goldens, so each value of the earlier file reached the file it belongs to: the stores,
/// the identity and the driver in the sibling's, the state store and the token path in the
/// children's, the REAPI pair and the client CA in the `[proxy]` table), the original is kept
/// byte for byte, root's 0600, in a 0700 directory, and written before the file it protects is
/// replaced. The operator token keeps its bytes and becomes `kbf-server`'s, which the earlier
/// layout's was not (the proxy's children could not read it). The output does not say that a
/// running kbf keeps the old file until it is restarted: the earlier unit is not to be restarted.
#[test]
fn an_earlier_host_is_split_into_the_new_files_and_its_original_is_kept() {
    let mut exec = earlier_host(EARLIER);
    let out = go_ok(&FakeOs::linux(), &mut exec, &minimal());
    assert_eq!(text(&exec, KBF_TOML), SIBLING_GOLDEN);
    assert_eq!(file_meta(&exec, KBF_TOML), (0o640, GROUP_KBF));
    assert_eq!(text(&exec, AIO_FRONTEND_TOML), FRONTEND_GOLDEN);
    assert_eq!(file_meta(&exec, AIO_FRONTEND_TOML), (0o640, GROUP_SERVER));
    assert_eq!(text(&exec, INSTALLER_TOML), INSTALLER_GOLDEN);
    assert_eq!(text(&exec, COPY), EARLIER);
    assert_eq!(file_meta(&exec, COPY), (0o600, ROOT));
    assert_eq!(dir_meta(&exec, COPY_DIR), (0o700, ROOT));
    let at = |path: &str| write_index(&exec, path).expect("written");
    assert!(at(COPY) < at(AIO_FRONTEND_TOML));
    assert!(at(COPY) < at(KBF_TOML));
    assert!(at(AIO_FRONTEND_TOML) < at(KBF_TOML));

    assert_eq!(text(&exec, API_TOKEN), OLD_TOKEN);
    assert_eq!(file_meta(&exec, API_TOKEN), (0o600, KBF_SERVER));
    assert!(!out.contains("first operator token"), "{out}");
    assert!(!out.contains(OLD_TOKEN.trim()), "{out}");
    assert!(!out.contains("a running kbf keeps the old one"), "{out}");
}

/// Catches the note about settings with no place printed when there are none: an earlier file
/// whose keys all have a place (no listeners, no worker certificate) is split with no such note,
/// and the commands are still shown.
#[test]
fn an_earlier_file_with_every_key_placed_gets_no_note_about_settings() {
    let mut bare = EARLIER.to_owned();
    for line in UNPLACED_LINES {
        bare = bare.replace(line, "");
    }
    let mut exec = earlier_host(&bare);
    let out = go_ok(&FakeOs::linux(), &mut exec, &minimal());
    assert!(!out.contains("holds settings"), "{out}");
    assert!(out.contains("migration 1/4"), "{out}");
    assert_eq!(text(&exec, KBF_TOML), SIBLING_GOLDEN);
    assert_eq!(text(&exec, COPY), bare);
}

/// Catches a second run that does work, or one that touches the original: the same flags on the
/// finished host plan nothing and the copy is as it was; a run that finds the copy made and the
/// file not yet replaced (a stop between the two) writes the one file it was missing and nothing
/// for the copy; and the short command of the first run, on the finished host, is refused for
/// the flags it lacks (pinned: the moved files are not read for values, as the setup doc says,
/// and this test holds that until it is wanted otherwise).
#[test]
fn a_second_run_does_nothing_new_and_a_stopped_run_resumes_without_the_copy() {
    let mut exec = earlier_host(EARLIER);
    go_ok(&FakeOs::linux(), &mut exec, &minimal());
    let done = exec.log.len();
    let os = settled(&UNITS);
    let again = go_ok(&os, &mut exec, &raw(Mode::AllInOne));
    assert_eq!(exec.log.len(), done, "{:?}", &exec.log[done..]);
    assert!(again.contains("\nnothing to do\n"), "{again}");
    assert_eq!(text(&exec, COPY), EARLIER);
    let err = go(&os, &mut exec, &minimal(), APPLY).expect_err("the flags are needed again");
    assert_eq!(err.exit(), Exit::Usage);
    let said = err.to_string();
    assert!(said.contains("--cold-store: required"), "{said}");
    assert_eq!(exec.log.len(), done, "{:?}", &exec.log[done..]);

    put(&mut exec, KBF_TOML, 0o640, GROUP_KBF, EARLIER);
    exec.log.clear();
    go_ok(&os, &mut exec, &minimal());
    assert_eq!(exec.log.len(), 1, "{:?}", exec.log);
    assert_eq!(write_index(&exec, KBF_TOML), Some(0));
    assert_eq!(text(&exec, KBF_TOML), SIBLING_GOLDEN);
    assert_eq!(text(&exec, COPY), EARLIER);
}

/// Catches an original lost: a copy that is kept and holds another file is not written over and
/// the new file is not written over the file it does not match; setup refuses (exit 5) naming the
/// copy, before it plans anything, dry run or apply.
#[test]
fn a_copy_of_another_original_is_refused_and_nothing_is_touched() {
    let other = "mode = \"all-in-one\"\n# another\n";
    for options in [APPLY, DRY] {
        let mut exec = earlier_host(EARLIER);
        put(&mut exec, COPY, 0o600, ROOT, other);
        let err = go(&FakeOs::linux(), &mut exec, &minimal(), options).expect_err("refused");
        assert_eq!(
            (err.exit(), err.code()),
            (Exit::NeedsOperator, "needs_operator")
        );
        let said = err.to_string();
        assert!(said.contains(COPY), "{said}");
        assert!(exec.log.is_empty(), "{:?}", exec.log);
        assert_eq!(text(&exec, COPY), other);
        assert_eq!(text(&exec, KBF_TOML), EARLIER);
        assert!(!exec.entries.contains_key(&hp(AIO_FRONTEND_TOML)));
    }
}

/// Catches an old value winning over a flag, or a flag lost: flags given on the run take
/// precedence over the earlier file's values (a node id, and the state database, which goes to
/// the children's file and to `installer.toml`), everything else is carried, and the original
/// keeps the old values. The cold store and the metadata directory given with the file's own
/// values are accepted.
#[test]
fn a_flag_of_the_run_overrides_the_old_value_and_the_rest_is_carried() {
    let database = "postgres:///kbf2?host=/run/postgresql&user=kbf-server";
    let mut flags = minimal();
    flags.kbf.node_id = some("farm-2");
    flags.kbf.cold_store = some("file:///var/lib/kbf/cold");
    flags.kbf.meta_dir = some("/var/lib/kbf-meta");
    flags.database = some(database);
    let mut exec = earlier_host(EARLIER);
    go_ok(&FakeOs::linux(), &mut exec, &flags);
    let sibling = SIBLING_GOLDEN.replace("farm-1", "farm-2");
    assert_eq!(text(&exec, KBF_TOML), sibling);
    let children = FRONTEND_GOLDEN.replace(DATABASE, database);
    assert_eq!(text(&exec, AIO_FRONTEND_TOML), children);
    let installer = INSTALLER_GOLDEN.replace(DATABASE, database);
    assert_eq!(text(&exec, INSTALLER_TOML), installer);
    assert_eq!(text(&exec, COPY), EARLIER);
}

/// Catches a missing setting read as a guess, and the earlier file's values escaping the usual
/// checks: a setting the flags and the earlier file both lack is required by its flag, and the
/// problems end with the line that says where an ungiven value came from; a carried key file is
/// held to the rules of a fresh host's (the REAPI key must belong to `kbf-server`, which the
/// earlier layout's did not), with the same line, and so is a REAPI certificate owned by `kbf`
/// (a certificate must be root's or `kbf-server`'s); nothing is written.
#[test]
fn what_is_missing_or_wrong_in_the_carried_values_is_refused_before_any_write() {
    let no_id = EARLIER.replace("node-id = \"farm-1\"\n", "");
    let mut exec = earlier_host(&no_id);
    let err = go(&FakeOs::linux(), &mut exec, &minimal(), APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::Usage);
    let said = err.to_string();
    assert!(
        said.contains("--node-id: required for --mode=all-in-one"),
        "{said}"
    );
    assert!(said.contains("the earlier all-in-one kbf.toml's"), "{said}");
    assert!(!said.contains("--cold-store"), "{said}");
    assert!(exec.log.is_empty(), "{:?}", exec.log);

    let mut exec = earlier_host(EARLIER);
    put(&mut exec, "/etc/kbf/reapi.key", 0o600, KBF, "key");
    let err = go(&FakeOs::linux(), &mut exec, &minimal(), APPLY).expect_err("refused");
    let said = err.to_string();
    assert!(
        said.contains("--reapi-tls-key: a key must be owned by"),
        "{said}"
    );
    assert!(said.contains("the earlier all-in-one kbf.toml's"), "{said}");
    assert!(exec.log.is_empty(), "{:?}", exec.log);

    let mut exec = earlier_host(EARLIER);
    put(&mut exec, "/etc/kbf/reapi.pem", 0o644, KBF, "cert");
    let err = go(&FakeOs::linux(), &mut exec, &minimal(), APPLY).expect_err("refused");
    let said = err.to_string();
    assert!(
        said.contains("--reapi-tls-cert: must be owned by root or the user"),
        "{said}"
    );
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches a flag that repoints the data of a host being moved: `--cold-store` or `--meta-dir`
/// naming another place than the earlier file does is refused (exit 5) with the flag named and
/// no value in the message, before anything is planned, and nothing is written.
#[test]
fn another_cold_store_or_meta_dir_than_the_files_is_refused_without_the_value() {
    let mut cold = minimal();
    cold.kbf.cold_store = some("file:///var/lib/kbf/other");
    let mut meta = minimal();
    meta.kbf.meta_dir = some("/var/lib/kbf-other");
    for (flags, flag) in [(cold, "--cold-store"), (meta, "--meta-dir")] {
        let mut exec = earlier_host(EARLIER);
        let err = go(&FakeOs::linux(), &mut exec, &flags, APPLY).expect_err("refused");
        assert_eq!(
            (err.exit(), err.code()),
            (Exit::NeedsOperator, "needs_operator")
        );
        let said = err.to_string();
        assert!(said.contains(flag), "{said}");
        for value in ["kbf/other", "kbf-other", "kbf/cold", "kbf-meta"] {
            assert!(!said.contains(value), "{value}: {said}");
        }
        assert!(exec.log.is_empty(), "{:?}", exec.log);
        assert_eq!(text(&exec, KBF_TOML), EARLIER);
    }
}

/// Catches a setting dropped silently, a value printed, or a carried key reported as lost: every
/// key with no place in the new layout is named in the output as `[table] key` (a top-level key by
/// its name), among them the listeners, the worker listener's certificate, the operator's own
/// table and a key of the layout whose value is not text; no value is printed; the keys that were
/// carried are not named; and the original, which keeps them all, is named.
#[test]
fn settings_with_no_place_in_the_new_layout_are_named_and_never_printed() {
    let tail = "\n[log]\nlog-files = 3\nlog-secret = \"hunter2-value\"\n";
    let extended = format!("extra-top = 1\n{EARLIER}{tail}");
    let extended = extended.replace("driver = \"container\"", "driver = 3");
    let mut exec = earlier_host(&extended);
    let earlier = aio_migrate::read(&exec, Platform::Linux)
        .expect("read")
        .expect("an earlier host");
    let mut names = earlier.unplaced();
    names.sort();
    assert_eq!(
        names,
        [
            "[daemon] driver",
            "[listener] api-listen",
            "[listener] listen",
            "[listener] worker-listen",
            "[log] log-files",
            "[log] log-secret",
            "[tls] worker-tls-cert",
            "[tls] worker-tls-key",
            "extra-top",
        ]
    );

    let out = go_ok(&FakeOs::linux(), &mut exec, &minimal());
    let line = out
        .lines()
        .find(|l| l.starts_with("note: /etc/kbf/kbf.toml holds settings"))
        .expect("a note");
    for name in names {
        assert!(line.contains(&name), "{name}: {line}");
    }
    for carried in ["mode", "cold-store", "node-id", "state-store"] {
        assert!(!line.contains(carried), "{carried}: {line}");
    }
    assert!(!line.contains("api-token-file"), "{line}");
    assert!(line.contains(COPY), "{line}");
    for value in ["hunter2-value", "0.0.0.0:8980"] {
        assert!(!out.contains(value), "{value}: {out}");
    }
    assert_eq!(text(&exec, COPY), extended);
    assert!(!text(&exec, KBF_TOML).contains("log-files"));
}

/// Catches setup acting on the services of the move, or telling the operator the wrong order: no
/// command it runs stops, starts, restarts, disables or names the earlier unit (the host says that
/// unit is enabled), the only command with `--now` is the enable of `kbf-installer.service` that
/// every run plans, and the output has the operator's commands in order: stop the earlier unit and
/// wait for it, disable it, reload systemd, then start the sibling and the proxy, with the exit 75
/// retry, the warning about a reboot in between and the warning not to restart the earlier unit.
#[test]
fn setup_stops_and_starts_none_of_the_move_and_prints_the_commands_in_order() {
    let mut os = FakeOs::linux();
    os.enabled.insert(OLD_UNIT.to_owned());
    let mut exec = earlier_host(EARLIER);
    let out = go_ok(&os, &mut exec, &minimal());
    let commands = cmds(&exec);
    for command in &commands {
        for word in ["stop", "start", "restart", "disable", OLD_UNIT] {
            assert!(!command.contains(word), "{word}: {command}");
        }
    }
    let now: Vec<&String> = commands.iter().filter(|c| c.contains("--now")).collect();
    assert_eq!(
        now,
        ["run /usr/bin/systemctl enable --now -- kbf-installer.service"]
    );
    let at = |what: &str| out.find(what).unwrap_or_else(|| panic!("{what}: {out}"));
    let first = format!("migration 1/4: systemctl stop {OLD_UNIT}, then wait");
    let second = format!("migration 2/4: systemctl disable {OLD_UNIT}");
    let places = [
        at(&first),
        at(&second),
        at("migration 3/4: systemctl daemon-reload"),
        at(FOURTH),
    ];
    assert!(places.windows(2).all(|w| w[0] < w[1]), "{places:?}");
    for want in ["exits 75", "do not reboot", "no time limit"] {
        assert!(out.contains(want), "{want}: {out}");
    }
    assert!(out.contains(THEN_PROXY), "{out}");
    assert!(out.contains("Do not restart"), "{out}");
}

/// Catches a dry run that writes, or one that leaves the move out of the plan: the copy, the
/// children's file and `kbf.toml` are named with `DRY-RUN:` and the commands are shown, and the
/// host is as it was (the earlier file, no copy, no directory for it, the token still `kbf`'s).
#[test]
fn a_dry_run_shows_the_split_and_changes_nothing() {
    let mut exec = earlier_host(EARLIER);
    let out = go(&FakeOs::linux(), &mut exec, &minimal(), DRY).expect("dry run");
    for file in [COPY, AIO_FRONTEND_TOML, KBF_TOML] {
        assert!(out.contains(&format!("DRY-RUN: write {file}")), "{out}");
    }
    assert!(out.contains("migration 1/4"), "{out}");
    assert!(exec.log.is_empty(), "{:?}", exec.log);
    assert_eq!(text(&exec, KBF_TOML), EARLIER);
    assert!(!exec.entries.contains_key(&hp(COPY)));
    assert!(!exec.entries.contains_key(&hp(COPY_DIR)));
    assert_eq!(file_meta(&exec, API_TOKEN), (0o600, KBF));
}

/// Catches the host named by the wrong error: a run without `--proxy` on an earlier host (the
/// command line that set it up) is told what the host is and what to add (exit 5), not only that
/// `--proxy` is required, and nothing is touched, dry run or apply.
#[test]
fn a_run_without_proxy_on_an_earlier_host_names_the_host_and_changes_nothing() {
    for options in [APPLY, DRY] {
        let mut exec = earlier_host(EARLIER);
        let mut bare = raw(Mode::AllInOne);
        bare.proxy.proxy = false;
        let err = go(&FakeOs::linux(), &mut exec, &bare, options).expect_err("refused");
        assert_eq!(
            (err.exit(), err.code()),
            (Exit::NeedsOperator, "needs_operator")
        );
        let said = err.to_string();
        assert!(said.contains(KBF_TOML), "{said}");
        assert!(said.contains("mode = \"all-in-one\""), "{said}");
        assert!(said.contains("give --proxy"), "{said}");
        assert!(exec.log.is_empty(), "{:?}", exec.log);
        assert_eq!(text(&exec, KBF_TOML), EARLIER);
    }
}

/// Catches an operator token lost in the move: a token at another path than the layout's (the
/// earlier file's `api-token-file`) is copied, with its bytes, to the layout's as `kbf-server`'s
/// and the old file is left; the output says so and not that a first token was made. With no token
/// anywhere (no key, or a key that names a file that is not there) the first one is made, as on a
/// fresh host.
#[test]
fn the_operator_token_of_the_earlier_file_is_kept_wherever_it_was() {
    let mut exec = released(host(), Platform::Linux);
    let earlier = token_named(OTHER_TOKEN);
    put(&mut exec, KBF_TOML, 0o640, GROUP_KBF, &earlier);
    put(&mut exec, OTHER_TOKEN, 0o600, KBF, "other-token-bytes\n");
    let out = go_ok(&FakeOs::linux(), &mut exec, &minimal());
    assert_eq!(text(&exec, API_TOKEN), "other-token-bytes\n");
    assert_eq!(file_meta(&exec, API_TOKEN), (0o600, KBF_SERVER));
    assert_eq!(text(&exec, OTHER_TOKEN), "other-token-bytes\n");
    assert_eq!(text(&exec, AIO_FRONTEND_TOML), FRONTEND_GOLDEN);
    let copied = format!("the operator token of {OTHER_TOKEN} is copied to {API_TOKEN}");
    assert!(out.contains(&copied), "{out}");
    assert!(!out.contains("first operator token"), "{out}");
    assert!(!out.contains("other-token-bytes"), "{out}");

    let no_key = EARLIER.replace("api-token-file = \"/etc/kbf/api-token\"\n", "");
    for old in [no_key, token_named("/etc/kbf/missing")] {
        let mut exec = released(host(), Platform::Linux);
        put(&mut exec, KBF_TOML, 0o640, GROUP_KBF, &old);
        let out = go_ok(&FakeOs::linux(), &mut exec, &minimal());
        assert_eq!(text(&exec, API_TOKEN), format!("{SECRET}\n"));
        assert_eq!(file_meta(&exec, API_TOKEN), (0o600, KBF_SERVER));
        assert!(out.contains("the first operator token is in"), "{out}");
    }
}

/// Catches an operator token dropped without a word: with a token at the layout's path and another
/// at the path `api-token-file` names, the layout's is kept (its bytes, `kbf-server`'s) and the
/// output names the other file as not used, never its bytes; with the same bytes in both files
/// there is nothing to say.
#[test]
fn a_second_token_that_differs_is_named_as_unused_and_an_equal_one_is_not() {
    let mut exec = earlier_host(&token_named(OTHER_TOKEN));
    put(&mut exec, OTHER_TOKEN, 0o600, KBF, "other-token-bytes\n");
    let out = go_ok(&FakeOs::linux(), &mut exec, &minimal());
    assert_eq!(text(&exec, API_TOKEN), OLD_TOKEN);
    assert_eq!(file_meta(&exec, API_TOKEN), (0o600, KBF_SERVER));
    assert_eq!(text(&exec, OTHER_TOKEN), "other-token-bytes\n");
    let unused = format!("the operator token of {OTHER_TOKEN} differs from {API_TOKEN}");
    assert!(out.contains(&unused), "{out}");
    assert!(!out.contains("other-token-bytes"), "{out}");
    assert!(!out.contains(OLD_TOKEN.trim()), "{out}");

    let mut exec = earlier_host(&token_named(OTHER_TOKEN));
    put(&mut exec, OTHER_TOKEN, 0o600, KBF, OLD_TOKEN);
    let out = go_ok(&FakeOs::linux(), &mut exec, &minimal());
    assert_eq!(text(&exec, API_TOKEN), OLD_TOKEN);
    assert!(!out.contains("the operator token of"), "{out}");
}
