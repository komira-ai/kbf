//! The state-store setting of a frontend: finding it in a kbf.toml or a unit file, rewriting
//! only its value, staging the backup and the new text, and putting them in place or back.
//! None of it needs a database.

mod support;

use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_state_copy::Error;
use kbf_state_copy::setting::{self, Found, Place};
use support::{
    FRONTEND_URL, Farm, backup_of, has, lacks, mode_of, read, read_text, scratch, sqlite_url,
    temp_of, unit_for, write_with_mode,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Reads the setting of `text`, put in a kbf.toml or in a unit file made for the purpose in
/// a directory of its own.
fn found_in(name: &str, text: &str, unit: bool) -> Result<Found, Error> {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = scratch(&format!("{name}-{}-{n}", std::process::id()));
    let file = dir.join("setting");
    write_with_mode(&file, text, 0o640);
    let place = if unit {
        Place::UnitFile(file)
    } else {
        Place::Config(file)
    };
    setting::find(&place)
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, Permissions::from_mode(mode)).expect("set the mode");
}

fn refusal_of(found: Result<Found, Error>) -> String {
    let error = found.expect_err("the setting is refused");
    assert_eq!(error.code(), 2, "{error}");
    error.to_string()
}

fn config_refusal(text: &str) -> String {
    refusal_of(found_in("setting-refused", text, false))
}

fn unit_refusal(text: &str) -> String {
    refusal_of(found_in("setting-unit-refused", text, true))
}

/// The value of the flag in the unit file `text`, and the text with the flag set to the
/// frontend's URL.
fn unit_flag(text: &str) -> (String, String) {
    let found = found_in("setting-unit-forms", text, true).expect("the setting");
    let rewritten = found.rewritten(FRONTEND_URL).expect("the new text");
    let text = String::from_utf8(rewritten).expect("UTF-8");
    (found.current, text)
}

/// Asserts that the rewrite of a kbf.toml is the old text with the quoted value replaced by
/// the new URL, and nothing else changed: comments, a tab, trailing spaces, a CR LF line end
/// and the missing final newline are all as they were, and the file itself is not touched.
#[test]
fn a_config_is_rewritten_in_its_value_alone() {
    let farm = Farm::golden("setting-config");
    let place = Place::Config(farm.config.clone());
    let found = setting::find(&place).expect("the setting");
    assert_eq!(found.current, sqlite_url(&farm.db));
    let rewritten = found.rewritten(FRONTEND_URL).expect("the new text");
    let url = sqlite_url(&farm.db);
    let expected = farm.text.replace(&url, FRONTEND_URL);
    assert_eq!(rewritten, expected.as_bytes());
    assert_ne!(expected, farm.text);
    assert_eq!(farm.config_now(), farm.text);
}

/// Asserts that a value in single quotes is found and written back in double quotes, that
/// the same key in another table is left alone, that a header with a trailing comment is a
/// header, and that the table after the scheduler's ends its search.
#[test]
fn only_the_key_of_the_scheduler_table_is_the_setting() {
    let text = "[other]\n\
        state-store = \"sqlite:///not/this\"\n\
        [[machines]]\n\
        state-store = \"sqlite:///nor/this\"\n\
        [scheduler] # the scheduler\n\
        state-store='sqlite:///var/lib/kbf/state.db'# note\n\
        [later]\n\
        state-store = \"sqlite:///nor/this\"\n";
    let found = found_in("setting-tables", text, false).expect("the setting");
    assert_eq!(found.current, "sqlite:///var/lib/kbf/state.db");
    let rewritten = found.rewritten(FRONTEND_URL).expect("the new text");
    let quoted = format!("\"{FRONTEND_URL}\"");
    let expected = text.replace("'sqlite:///var/lib/kbf/state.db'", &quoted);
    assert_eq!(rewritten, expected.as_bytes());
}

/// Asserts that a kbf.toml with no string `[scheduler] state-store`, one that is not TOML,
/// and one that is not text are each refused with status 2 and the reason, and that a file
/// that is not there fails with status 1 naming it.
#[test]
fn a_config_without_a_setting_to_edit_is_refused() {
    let why = config_refusal("mode = \"frontend\"\n");
    has(&why, "has no [scheduler] state-store");
    let why = config_refusal("[scheduler]\nstate-store = 5\n");
    has(&why, "with a string value");
    let why = config_refusal("[scheduler\n");
    has(&why, "is not valid TOML");
    let why = config_refusal("[scheduler]\n\"state-store\" = \"sqlite:///x\"\n");
    has(&why, "and found 0");
    let why = config_refusal("[scheduler]\nstate-store = \"\"\"sqlite:///x\"\"\"\n");
    has(&why, "and found 0");
    let dir = scratch("setting-binary");
    let file = dir.join("setting");
    fs::write(&file, [0xff, 0xfe, 0x00]).expect("write bytes");
    let why = refusal_of(setting::find(&Place::Config(file)));
    has(&why, "is not UTF-8 text");
    let missing = Place::Config(dir.join("missing.toml"));
    let error = setting::find(&missing).expect_err("no such file");
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "missing.toml");
}

/// Asserts that a key written twice, once inside a multi-line string, is refused as two
/// places to edit, and that a lookalike line inside a string, with the real key written in
/// a form the line scan cannot read, is found but refused when the new text does not parse
/// to the old one with only the value changed.
#[test]
fn a_line_that_only_looks_like_the_setting_is_not_edited() {
    let twice = "[scheduler]\nstate-store = \"sqlite:///x\"\nnote = \"\"\"\n\
        state-store = \"sqlite:///y\"\n\"\"\"\n";
    let why = config_refusal(twice);
    has(&why, "needs exactly one line");
    has(&why, "and found 2");
    let masked = "[scheduler]\n\"state-store\" = \"sqlite:///x\"\nnote = \"\"\"\n\
        state-store = \"sqlite:///y\"\n\"\"\"\n";
    let found = found_in("setting-masked", masked, false).expect("one line");
    let error = found.rewritten(FRONTEND_URL).expect_err("unsafe");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "cannot be rewritten safely");
}

/// Asserts that a unit file's flag is rewritten in its value alone, and that the value is
/// escaped for a unit file (`%` and `$` doubled).
#[test]
fn a_unit_file_is_rewritten_in_its_flag_alone() {
    let farm = Farm::golden("setting-unit");
    let text = unit_for(&farm.db);
    let found = found_in("setting-unit-file", &text, true).expect("the setting");
    assert_eq!(found.current, sqlite_url(&farm.db));
    let url = "postgres:///kbf?host=/run/postgresql&application_name=a%2Fb$c";
    let rewritten = found.rewritten(url).expect("the new text");
    let escaped = "postgres:///kbf?host=/run/postgresql&application_name=a%%2Fb$$c";
    let expected = text.replace(&sqlite_url(&farm.db), escaped);
    assert_eq!(rewritten, expected.as_bytes());
    has(&expected, "--listen=0.0.0.0:8980");
}

/// Asserts that the flag is found wherever it is: at the start of a line, on a continuation
/// line, at the end of a text with no newline; and that the value is read back unescaped.
#[test]
fn a_flag_is_found_wherever_it_is_and_read_back_unescaped() {
    let (current, new) = unit_flag("--state-store=sqlite:///x\n");
    assert_eq!(current, "sqlite:///x");
    assert_eq!(new, format!("--state-store={FRONTEND_URL}\n"));
    let (current, new) = unit_flag("ExecStart=kbf \\\n  --state-store=sqlite:///x \\\n  --b\n");
    assert_eq!(current, "sqlite:///x");
    let want = format!("ExecStart=kbf \\\n  --state-store={FRONTEND_URL} \\\n  --b\n");
    assert_eq!(new, want);
    let (current, new) = unit_flag("ExecStart=kbf --state-store=sqlite:///100%%/x");
    assert_eq!(current, "sqlite:///100%/x");
    let want = format!("ExecStart=kbf --state-store={FRONTEND_URL}");
    assert_eq!(new, want);
    let (current, _new) = unit_flag("ExecStart=kbf --state-store=sqlite:///a$$b");
    assert_eq!(current, "sqlite:///a$b");
}

/// Asserts that a unit file with no `--state-store=` that can be edited is refused with
/// status 2 saying how many were found: none at all, one in a comment, one inside quotes,
/// one glued to the word before it, the space-separated form, and two flags.
#[test]
fn a_unit_file_without_one_flag_to_edit_is_refused() {
    let why = unit_refusal("[Service]\nExecStart=/usr/bin/kbf --mode=frontend\n");
    has(&why, "and found 0");
    has(&why, "--state-store=URL");
    let why = unit_refusal("# --state-store=sqlite:///x\n; --state-store=sqlite:///x\n");
    has(&why, "and found 0");
    let why = unit_refusal("ExecStart=kbf \"--state-store=sqlite:///x\"\n");
    has(&why, "and found 0");
    let why = unit_refusal("ExecStart=kbf x--state-store=sqlite:///x\n");
    has(&why, "and found 0");
    let why = unit_refusal("ExecStart=kbf --state-store sqlite:///x\n");
    has(&why, "and found 0");
    let why = unit_refusal("ExecStart=kbf --state-store=a:///x --state-store=b:///y\n");
    has(&why, "and found 2");
}

/// Asserts that a URL the frontend would take is returned trimmed, that a URL with a
/// password (in the user information or as a parameter) is refused with status 2 without
/// printing it, and that a URL the store does not take fails with status 1.
#[test]
fn a_url_with_a_password_or_one_the_store_refuses_is_not_the_setting() {
    let url = format!("  {FRONTEND_URL}\n");
    let checked = setting::check_url(&url).expect("a good URL");
    assert_eq!(checked, FRONTEND_URL);
    for url in [
        "postgres://u:s3cret-pw@h/db",
        "postgres://h/db?password=s3cret-pw",
    ] {
        let error = setting::check_url(url).expect_err("a password");
        assert_eq!(error.code(), 2);
        has(&error.to_string(), "--frontend-url");
        lacks(&error.to_string(), "s3cret-pw");
    }
    let error = setting::check_url("mysql://h/db").expect_err("not Postgres");
    assert_eq!(error.code(), 1);
}

/// Asserts that a URL with a space, a quote, a backslash or a character outside ASCII is
/// refused with status 2, because a setting file would have to quote it.
#[test]
fn a_url_that_a_setting_file_would_have_to_quote_is_refused() {
    for url in [
        "postgres://h/db?application_name=a b",
        "postgres://h/db?application_name=a\"b",
        "postgres://h/db?application_name=a'b",
        "postgres://h/db?application_name=a\\b",
        "postgres://h/db?application_name=caf\u{e9}",
    ] {
        let error = setting::check_url(url).expect_err("a character to quote");
        assert_eq!(error.code(), 2, "{url}");
        has(&error.to_string(), "would have to quote");
    }
}

/// Asserts that a setting names the state file being copied when it is that file, also by a
/// link, and is refused with status 2 when it is not a `sqlite://` URL (without quoting
/// what it is), names a file that cannot be read, or names another file.
#[test]
fn the_setting_must_name_the_state_file_being_copied() {
    let farm = Farm::golden("setting-names");
    let db = fs::canonicalize(&farm.db).expect("the file is there");
    let link = farm.dir.join("link.db");
    symlink(&db, &link).expect("make a link");
    let other = farm.dir.join("other.db");
    fs::copy(&db, &other).expect("copy the file");
    setting::names_file(&sqlite_url(&db), &db).expect("the same file");
    setting::names_file(&sqlite_url(&link), &db).expect("the same file by a link");
    let postgres = setting::names_file("postgres://u:s3cret-pw@h/db", &db);
    let error = postgres.expect_err("not a sqlite:// URL");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "not a sqlite:// URL");
    lacks(&error.to_string(), "s3cret-pw");
    let relative = setting::names_file("sqlite://state.db", &db);
    let error = relative.expect_err("not an absolute path");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "with an absolute path");
    let missing = setting::names_file("sqlite:///no/such/state.db", &db);
    let error = missing.expect_err("no such file");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "/no/such/state.db");
    let moved = setting::names_file(&sqlite_url(&other), &db);
    let error = moved.expect_err("another file");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "other.db, not ");
}

/// Asserts that a backup of an earlier cutover is refused whatever it is (here, a link that
/// leads nowhere), that so is a temporary file left by an interrupted cutover, and that
/// neither being there is fine.
#[test]
fn a_backup_that_is_there_is_refused() {
    let farm = Farm::golden("setting-backup");
    let place = Place::Config(farm.config.clone());
    let found = setting::find(&place).expect("the setting");
    found.require_no_backup().expect("there is no backup");
    let backup = backup_of(&found.file);
    symlink("nowhere", &backup).expect("make a link");
    let error = found.require_no_backup().expect_err("a link");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "pre-postgres exists");
    assert_eq!(found.backup(), backup);
    fs::remove_file(&backup).expect("remove the link");
    found.require_no_backup().expect("the backup is gone");
    fs::write(temp_of(&found.file), "left by a crash").expect("write the leftover");
    let error = found.require_no_backup().expect_err("a leftover file");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "postgres-new exists");
}

fn staged_farm(name: &str, mode: u32) -> (Farm, Found, Vec<u8>) {
    let farm = Farm::golden(name);
    write_with_mode(&farm.config, &farm.text, mode);
    let place = Place::Config(farm.config.clone());
    let found = setting::find(&place).expect("the setting");
    let rewritten = found.rewritten(FRONTEND_URL).expect("the new text");
    (farm, found, rewritten)
}

/// Asserts that staging makes the backup (the file byte for byte) and the new text beside
/// the file, each with the file's mode whatever its bits (and not the umask's), leaves the
/// file as it was, and that dropping what was staged removes both.
#[test]
fn staging_makes_two_files_and_dropping_them_removes_both() {
    for mode in [0o600, 0o640, 0o644] {
        let (farm, found, rewritten) = staged_farm("setting-stage", mode);
        let before = farm.names();
        let staged = found.stage(&rewritten).expect("the files are staged");
        let (backup, temp) = (found.backup(), temp_of(&found.file));
        assert_eq!(read(&backup), farm.text.as_bytes());
        assert_eq!(read(&temp), rewritten);
        assert_eq!(mode_of(&backup), mode);
        assert_eq!(mode_of(&temp), mode);
        assert_eq!(farm.config_now(), farm.text);
        assert_eq!(farm.names().len(), before.len() + 2);
        drop(staged);
        assert_eq!(farm.names(), before);
    }
}

/// Asserts that putting the staged text in place makes the file the new text with its mode
/// kept, keeps the backup byte for byte, leaves no temporary file, and that restoring the
/// backup makes the file the original again, byte for byte, with the backup gone.
#[test]
fn the_new_text_goes_over_the_file_and_the_backup_goes_back() {
    let (farm, found, rewritten) = staged_farm("setting-switch", 0o640);
    let before = farm.names();
    let staged = found.stage(&rewritten).expect("the files are staged");
    staged.put_in_place(&found).expect("the text is in place");
    assert_eq!(read(&farm.config), rewritten);
    assert_eq!(mode_of(&farm.config), 0o640);
    assert_eq!(read(&found.backup()), farm.text.as_bytes());
    assert_eq!(farm.names().len(), before.len() + 1);
    let place = Place::Config(farm.config.clone());
    let restore = setting::find_backup(&place).expect("a backup");
    restore.apply().expect("the backup is restored");
    assert_eq!(read(&farm.config), farm.text.as_bytes());
    assert_eq!(mode_of(&farm.config), 0o640);
    assert_eq!(farm.names(), before);
}

/// Asserts that a name that is taken fails the staging with status 1 and leaves what was
/// there: a leftover temporary file stays, and the backup made before it is removed; a file
/// already at the backup's name stays, and nothing is made.
#[test]
fn a_name_that_is_taken_fails_the_staging_and_nothing_else_is_left() {
    let (farm, found, rewritten) = staged_farm("setting-taken", 0o640);
    let before = farm.names();
    let temp = temp_of(&found.file);
    fs::write(&temp, "left by a crash").expect("write the leftover");
    let error = found.stage(&rewritten).expect_err("the temp name");
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "postgres-new");
    assert_eq!(read_text(&temp), "left by a crash");
    assert_eq!(farm.names().len(), before.len() + 1);
    fs::remove_file(&temp).expect("remove the leftover");
    fs::write(found.backup(), "someone else's").expect("write the other file");
    let error = found.stage(&rewritten).expect_err("the backup name");
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "pre-postgres");
    assert_eq!(read_text(&found.backup()), "someone else's");
    assert_eq!(farm.names().len(), before.len() + 1);
}

/// Asserts that a rename of the new text that fails (here, the file's name is a directory by
/// then) fails the switch with status 1, says that the target holds the copy all the same,
/// and leaves neither the backup nor the new text in the directory.
#[test]
fn a_rename_that_fails_leaves_no_staged_file() {
    let (farm, found, rewritten) = staged_farm("setting-rename-fails", 0o640);
    let before = farm.names();
    let staged = found.stage(&rewritten).expect("the files are staged");
    fs::remove_file(&found.file).expect("remove the file");
    fs::create_dir(&found.file).expect("make a directory in its place");
    let error = staged.put_in_place(&found).expect_err("the rename fails");
    assert_eq!(error.code(), 1);
    let said = error.to_string();
    has(&said, "the target already holds the copy");
    assert_eq!(farm.names(), before);
}

/// Asserts that a rename that was done, followed by a directory that cannot be synced (here,
/// one that cannot be opened for reading), fails the switch with status 1 saying the rename
/// was done, with the new text in place and the backup kept. It needs an account that is not
/// root, as the tests run under.
#[test]
fn a_directory_that_cannot_be_synced_fails_after_the_rename() {
    let (farm, found, rewritten) = staged_farm("setting-sync-fails", 0o640);
    let staged = found.stage(&rewritten).expect("the files are staged");
    let mode = mode_of(&farm.dir);
    set_mode(&farm.dir, 0o300);
    let result = staged.put_in_place(&found);
    set_mode(&farm.dir, mode);
    let error = result.expect_err("the directory cannot be synced");
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "the rename itself was done");
    assert_eq!(read(&farm.config), rewritten);
    assert_eq!(read(&found.backup()), farm.text.as_bytes());
}

/// Asserts that a restore whose rename fails (here, the file's name is a directory) fails
/// with status 1 and leaves the backup where it was.
#[test]
fn a_restore_whose_rename_fails_keeps_the_backup() {
    let farm = Farm::golden("setting-restore-fails");
    let place = Place::Config(farm.config.clone());
    let canonical = fs::canonicalize(&farm.config).expect("the file is there");
    write_with_mode(&backup_of(&canonical), &farm.text, 0o640);
    let restore = setting::find_backup(&place).expect("a backup");
    fs::remove_file(&restore.file).expect("remove the file");
    fs::create_dir(&restore.file).expect("make a directory in its place");
    let before = farm.names();
    let error = restore.apply().expect_err("the rename fails");
    assert_eq!(error.code(), 1);
    assert_eq!(read_text(&restore.backup), farm.text);
    assert_eq!(farm.names(), before);
}

/// Asserts that a restore with no backup is refused with status 2 naming the backup, that
/// so is one where the backup's name is a directory, and that a setting file that is not
/// there fails with status 1.
#[test]
fn a_restore_needs_a_backup_that_is_a_file() {
    let farm = Farm::golden("setting-restore");
    let place = Place::Config(farm.config.clone());
    let error = setting::find_backup(&place).expect_err("no backup");
    assert_eq!(error.code(), 2);
    let said = error.to_string();
    has(&said, "pre-postgres does not exist as a regular file");
    let canonical = fs::canonicalize(&farm.config).expect("the file is there");
    fs::create_dir(backup_of(&canonical)).expect("make a directory");
    let error = setting::find_backup(&place).expect_err("a directory");
    assert_eq!(error.code(), 2);
    let missing = Place::Config(farm.dir.join("missing.toml"));
    let error = setting::find_backup(&missing).expect_err("no such file");
    assert_eq!(error.code(), 1);
}

/// Asserts that a place reports the setting it is about and what to do next: a config says
/// to start the frontend, a unit file to reload systemd first.
#[test]
fn a_place_says_what_it_is_and_what_comes_next() {
    let config = Place::Config("kbf.toml".into());
    let unit = Place::UnitFile("override.conf".into());
    assert_eq!(config.path(), Path::new("kbf.toml"));
    assert_eq!(unit.path(), Path::new("override.conf"));
    has(config.name(), "[scheduler]");
    has(unit.name(), "--state-store=");
    assert_eq!(config.next_step(), "start the frontend");
    has(unit.next_step(), "systemctl daemon-reload");
}
