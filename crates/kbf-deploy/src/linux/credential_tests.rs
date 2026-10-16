//! Rule `credential`'s reading of a unit: each way of handing a build node the key is
//! seen, and the near misses (another file, another variable, a reset) are not.

use std::path::Path;

use kbf_host::exec::Entry;
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;

use super::*;

const KEY: &str = "/etc/kbf/s3.env";

fn found(service: &str) -> Vec<String> {
    let unit = UnitFile::parse(&format!("[Service]\n{service}")).expect("a unit");
    key_given(&unit, Path::new(KEY))
}

/// Catches an `EnvironmentFile=` that reaches the key file by another spelling going
/// unseen: optional (`-`), quoted, through `.`, `..` or `//`, with a `\` glob(3) takes
/// off, or a wildcard that matches it (in the directory or the name, `?` and `[...]`
/// too, a `[:class:]` in one); and a file that is not the key flagged (another name, a
/// pattern that cannot match, `*` across `/`; a name ending in a lone `\` cannot be
/// written: systemd reads it as a continued line). Around a trailing `/`: the name
/// before it is the key file (glob(3) lists it), a `/.` after it or a wildcard or `\`
/// in it are not; U+00A0 around the path is part of it; a relative path is ignored.
#[test]
fn environment_files_that_name_the_key_are_seen_however_written() {
    for value in [
        "/etc/kbf/s3.env",
        "-/etc/kbf/s3.env",
        "\"/etc/kbf/s3.env\"",
        "-'/etc/kbf/s3.env'",
        "/etc/kbf/./s3.env",
        "/etc//kbf/s3.env",
        "/etc/other/../kbf/s3.env",
        "/etc/kbf/*.env",
        "/etc/kbf/*",
        "/etc/*/s3.env",
        "/etc/kbf/s3.e?v",
        "/etc/kbf/s[0-9].env",
        "/etc/kbf/[!a]3.env",
        "/root/s3.env",
        "/etc/kbf/./*.env",
        "/etc//kbf/s?.env",
        "/etc/other/../kbf/s3*",
        "/etc/kbf/[xys]3.env",
        "/etc/kbf/[[:alpha:]]3.env",
        "/etc/kbf/\\s3.env",
        // glob(3) expands `pattern/` as `pattern` and keeps a name with no `\` or
        // wildcard even when it is a file: systemd reads the key.
        "/etc/kbf/s3.env/",
        "/etc/kbf/s3.env//",
        "/etc/*/s3.env/",
        "/root/s3.env/",
        "/etc/kbf/s3.env\\/",
    ] {
        assert_eq!(
            found(&format!("EnvironmentFile={value}\n")),
            [format!("EnvironmentFile={value}")],
            "{value}"
        );
    }
    for value in [
        "/etc/kbf/daemon.env",
        "/etc/kbf/s3.env.example.d/daemon.env",
        "/etc/*.env",
        "/etc/kbf/[!s]3.env",
        "/etc/kbf/s3.env.d/*",
        "/etc/kbf/[ab",
        "/srv/*/s[3].env",
        "/etc/kbf/s3.env[0-9]",
        "/etc/kbf/s3.env?",
        "/etc/kbf[",
        "/etc[/]kbf/s3.env",
        "/etc/kbf/[s]4.env",
        "/etc/kbf/[t-z]3.env",
        "/etc/kbf/[a-r]3.env",
        "/etc/kbf/[[:digit:]]3.env",
        // A `.` or `..` after the name asks for a directory: the kernel answers
        // ENOTDIR, glob(3) lists nothing, and systemd reads no file.
        "/etc/kbf/s3.env/.",
        "/etc/kbf/s3.env/./",
        "/root/s3.env/.",
        "/root/s3.env/..",
        // A wildcard or a `\` in the name before a `/`: glob(3) reads the directory
        // for directories only (GLOB_ONLYDIR), so it never lists the key file.
        "/etc/kbf/s3*/",
        "/etc/kbf/*.env/",
        "/etc/kbf/\\s3.env/",
        "/root/\\s3.env/",
        // U+00A0 is not systemd's whitespace: around the path it makes another path,
        // one that is not absolute (ignored) or names another file.
        "\u{a0}/etc/kbf/s3.env",
        "/etc/kbf/s3.env\u{a0}",
        "-\u{a0}/root/s3.env",
        // systemd ignores a relative path.
        "root/s3.env",
    ] {
        assert_eq!(
            found(&format!("EnvironmentFile={value}\n")),
            Vec::<String>::new(),
            "{value}"
        );
    }
}

/// Catches systemd's resets not followed: an empty `EnvironmentFile=`, `Environment=`
/// or `PassEnvironment=` drops what came before it (a drop-in that clears the unit's
/// key file passes), and a later non-empty one counts again.
#[test]
fn an_empty_assignment_resets_its_list() {
    assert!(found("EnvironmentFile=/etc/kbf/s3.env\nEnvironmentFile=\n").is_empty());
    assert!(found("Environment=AWS_PROFILE=x\nEnvironment=\n").is_empty());
    assert!(found("PassEnvironment=AWS_PROFILE\nPassEnvironment=\n").is_empty());
    assert_eq!(
        found("EnvironmentFile=\nEnvironmentFile=/etc/kbf/s3.env\n"),
        ["EnvironmentFile=/etc/kbf/s3.env"]
    );
    assert!(found("LoadCredential=k:/etc/kbf/s3.env\nLoadCredentialEncrypted=\n").is_empty());
    assert!(found("SetCredential=k:v\nSetCredentialEncrypted=\n").is_empty());
    assert!(found("ImportCredential=aws.*\nImportCredential=\n").is_empty());
    // One group's reset leaves the others.
    assert_eq!(
        found("LoadCredential=k:/x\nSetCredential=\n"),
        ["LoadCredential=k:/x, a systemd credential"]
    );
}

/// Catches an `AWS_*` variable missed when it is not alone on the line, is quoted
/// (around the assignment or inside it), or is one of the variables that point an SDK
/// at a key elsewhere (`AWS_PROFILE`, `AWS_SHARED_CREDENTIALS_FILE`, `AWS_CONFIG_FILE`)
/// or carry one (`AWS_SESSION_TOKEN`); and a value that merely mentions `AWS_` flagged,
/// or one split at a U+00A0 (systemd splits at its WHITESPACE only).
#[test]
fn environment_assignments_are_unquoted_as_systemd_does() {
    let rows = [
        ("Environment=AWS_ACCESS_KEY_ID=a\n", "AWS_ACCESS_KEY_ID"),
        (
            "Environment=A=1 AWS_SECRET_ACCESS_KEY=b B=2\n",
            "AWS_SECRET_ACCESS_KEY",
        ),
        (
            "Environment=\"AWS_SESSION_TOKEN=x y\"\n",
            "AWS_SESSION_TOKEN",
        ),
        ("Environment='AWS_PROFILE=prod' X=1\n", "AWS_PROFILE"),
        (
            "Environment=AWS_SHARED_CREDENTIALS_FILE=\"/x y\"\n",
            "AWS_SHARED_CREDENTIALS_FILE",
        ),
        (
            "Environment=X=\"a b\" AWS_CONFIG_FILE=/c\n",
            "AWS_CONFIG_FILE",
        ),
        (
            "Environment=\"X=a AWS_PROFILE=b\" AWS_REGION=r\n",
            "AWS_REGION",
        ),
    ];
    for (line, name) in rows {
        assert_eq!(found(line), [format!("Environment= sets {name}")], "{line}");
    }
    assert!(found("Environment=\"X=a AWS_PROFILE=b\"\n").is_empty());
    assert!(found("Environment=X=AWS_PROFILE=b\n").is_empty());
    assert!(found("Environment=XDG_RUNTIME_DIR=/run/user/1500\n").is_empty());
    // Words split at systemd's WHITESPACE only: U+00A0 is inside a word.
    assert!(found("Environment=X=a\u{a0}AWS_PROFILE=b\n").is_empty());
    assert!(found("PassEnvironment=PATH\u{a0}AWS_PROFILE\n").is_empty());
    assert_eq!(
        found("Environment=X=a\tAWS_PROFILE=b\n"),
        ["Environment= sets AWS_PROFILE"]
    );
    // Not [Service]: not the process's environment.
    let unit = UnitFile::parse("[Unit]\nEnvironment=AWS_PROFILE=x\n").expect("a unit");
    assert!(key_given(&unit, Path::new(KEY)).is_empty());
}

/// Catches `PassEnvironment=` and the systemd credential directives not read: each
/// hands the service a secret the rule cannot see into, so each is refused.
#[test]
fn passed_variables_and_systemd_credentials_are_refused() {
    assert_eq!(
        found("PassEnvironment=PATH AWS_SECRET_ACCESS_KEY\n"),
        ["PassEnvironment= passes AWS_SECRET_ACCESS_KEY"]
    );
    assert!(found("PassEnvironment=PATH LANG\n").is_empty());
    for key in [
        "LoadCredential",
        "LoadCredentialEncrypted",
        "SetCredential",
        "SetCredentialEncrypted",
        "ImportCredential",
    ] {
        assert_eq!(
            found(&format!("{key}=s3:/etc/kbf/s3.env\n")),
            [format!("{key}=s3:/etc/kbf/s3.env, a systemd credential")],
            "{key}"
        );
    }
}

fn file(exec: &mut FakeExec, path: &str, text: &str) {
    let entry = Entry::File {
        perms: Perms::new(0o600),
        owner: Owner { uid: 0, gid: 0 },
        bytes: text.as_bytes().to_vec(),
    };
    exec.entries
        .insert(HostPath::new(path).expect("absolute"), entry);
}

/// Catches a copy of the key under another name passed (the file's variables not
/// read, read with a comment or an `export ` taken for a name, or a `\` in its path
/// kept, a trailing `/` not dropped, or a `\.` read before its `\` is taken off), a
/// file read through a `/.` systemd cannot open, a link to it passed (it is not
/// followed, so it is refused), a wildcard or an unreadable file passed
/// without a word, and the key file itself reported twice (offline and on the host).
#[test]
fn on_the_host_each_environment_file_is_read() {
    let mut exec = FakeExec::new(0);
    file(
        &mut exec,
        "/etc/kbf/copy.env",
        "# AWS_X=no\nA=1\nAWS_SECRET_ACCESS_KEY=k\n",
    );
    file(&mut exec, "/etc/kbf/exported.env", "export AWS_PROFILE=p\n");
    file(
        &mut exec,
        "/etc/kbf/own.env",
        "; AWS_X\nRUST_LOG=info\nnot an assignment\n",
    );
    file(&mut exec, KEY, "AWS_ACCESS_KEY_ID=a\n");
    exec.entries.insert(
        HostPath::new("/etc/kbf/link.env").expect("absolute"),
        Entry::Other,
    );
    let unit = UnitFile::parse(
        "[Service]\n\
         EnvironmentFile=-/etc/kbf/copy.env\n\
         EnvironmentFile=/etc/kbf/./exported.env\n\
         EnvironmentFile=/etc/kbf/own.env\n\
         EnvironmentFile=/etc/kbf/missing.env\n\
         EnvironmentFile=relative.env\n\
         EnvironmentFile=/etc/kbf/link.env\n\
         EnvironmentFile=/etc/kbf/c\\opy.env\n\
         EnvironmentFile=/etc/kbf/copy.env/\n\
         EnvironmentFile=/etc/kbf/copy.env/.\n\
         EnvironmentFile=/etc/kbf/\\./copy.env\n\
         EnvironmentFile=/etc/kbf/*.conf\n\
         EnvironmentFile=/unreadable\n\
         EnvironmentFile=/etc/kbf/s3.env\n",
    )
    .expect("a unit");
    let r = on_host(&exec, &unit, Path::new(KEY));
    assert_eq!(
        r.failures,
        [
            "EnvironmentFile=-/etc/kbf/copy.env, which sets AWS_SECRET_ACCESS_KEY",
            "EnvironmentFile=/etc/kbf/./exported.env, which sets AWS_PROFILE",
            "EnvironmentFile=/etc/kbf/link.env, a link or special file the check does not follow (name the file itself)",
            "EnvironmentFile=/etc/kbf/c\\opy.env, which sets AWS_SECRET_ACCESS_KEY",
            "EnvironmentFile=/etc/kbf/copy.env/, which sets AWS_SECRET_ACCESS_KEY",
            "EnvironmentFile=/etc/kbf/\\./copy.env, which sets AWS_SECRET_ACCESS_KEY",
        ]
    );
    assert_eq!(
        r.notes,
        [
            "SKIP credential: EnvironmentFile=/etc/kbf/*.conf is a wildcard; the files it matches were not read",
            "SKIP credential: cannot read EnvironmentFile=/unreadable (permission denied), so its variables were not checked",
        ]
    );
}

/// Catches the path reading behind the rule: `..` above the root, a trailing `/`, and
/// words split at any Unicode space rather than systemd's WHITESPACE.
#[test]
fn paths_are_normalised_by_their_words() {
    assert_eq!(
        normalise("/../etc//kbf/./s3.env/"),
        Path::new("/etc/kbf/s3.env")
    );
    assert_eq!(normalise("a/../b"), Path::new("b"));
    assert_eq!(normalise("./a"), Path::new("a"));
    assert_eq!(
        words("a 'b c' \"d\\\"e\" f\\ g h''"),
        ["a", "b c", "d\"e", "f g", "h"]
    );
    assert!(words("  ").is_empty());
    assert_eq!(words("a\tb\rc\nd e"), ["a", "b", "c", "d", "e"]);
    assert_eq!(words("a\u{a0}b \u{a0}"), ["a\u{a0}b", "\u{a0}"]);
    assert_eq!(words("''"), [""]);
}
