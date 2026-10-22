use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;

use super::{Allowlist, ChannelSource};
use crate::outcome::Refusal;
use crate::request::Channel;
use crate::trust::Trust;

struct Fixture {
    dir: TestDir,
    trust: Trust,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let trust = Trust::owned_by(fs::metadata(&*dir).unwrap().uid());
        Self { dir, trust }
    }

    fn write(&self, name: &str, mode: u32, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// A key file, owned by the trusted user, mode 0644.
    fn key(&self) -> PathBuf {
        self.write("key.gpg", 0o644, "key")
    }

    /// A deb822 source naming `signed_by`.
    fn deb822(&self, signed_by: &str) -> PathBuf {
        let text = format!(
            "Types: deb\nURIs: https://apt.example.org/kbf\nSuites: candidate\nComponents: main\nSigned-By: {signed_by}\n"
        );
        self.write("candidate.sources", 0o644, &text)
    }

    /// The allowlist naming `candidate` with `source`.
    fn allowlist(&self, source: &Path) -> Allowlist {
        let text = format!(
            "[channels.candidate]\nsuite = \"candidate\"\nsource_list = \"{}\"\n",
            source.display()
        );
        let path = self.write("host-install.toml", 0o644, &text);
        Allowlist::load(&self.trust, &path).unwrap()
    }

    fn channel(&self, source: &Path) -> Result<ChannelSource, Refusal> {
        self.allowlist(source)
            .channel(&self.trust, &Channel::new("candidate").unwrap())
    }
}

/// A signed deb822 source whose key is root's: the channel is usable, with its suite and
/// source file as configured.
#[test]
fn a_signed_channel_is_usable() {
    let f = Fixture::new("config-ok");
    let key = f.key();
    let source = f.deb822(&key.display().to_string());
    assert_eq!(
        f.channel(&source),
        Ok(ChannelSource {
            suite: "candidate".into(),
            source_list: source
        })
    );
}

/// The one-line form, with options before the URI and a comment line, is read too.
#[test]
fn a_one_line_source_is_read() {
    let f = Fixture::new("config-oneline");
    let key = f.key();
    let text = format!(
        "# kbf\ndeb [arch=amd64 signed-by={}] https://apt.example.org/kbf candidate main\n",
        key.display()
    );
    let source = f.write("candidate.list", 0o644, &text);
    assert!(f.channel(&source).is_ok());
    let two = format!(
        "{text}deb-src [signed-by={}] https://apt.example.org/kbf candidate main\n",
        key.display()
    );
    let source = f.write("two.list", 0o644, &two);
    assert!(f.channel(&source).is_ok(), "deb and deb-src, both signed");
}

/// Catches a channel that is not refused when its source names no `signed-by`: each shape of
/// an unsigned source, and an entry among signed ones that lacks it.
#[test]
fn a_source_without_signed_by_is_refused() {
    let f = Fixture::new("config-unsigned");
    let key = f.key();
    let k = key.display();
    let cases = [
        (
            "deb822, no field",
            "Types: deb\nURIs: https://apt.example.org/kbf\nSuites: candidate\n".to_owned(),
        ),
        (
            "one-line, no option",
            "deb https://apt.example.org/kbf candidate main\n".to_owned(),
        ),
        (
            "one-line, other options",
            "deb [arch=amd64] https://apt.example.org/kbf candidate main\n".to_owned(),
        ),
        (
            "one-line, deb-src",
            "deb-src https://apt.example.org/kbf candidate main\n".to_owned(),
        ),
        ("empty file", String::new()),
        (
            "comments only",
            "# deb [signed-by=/x] https://a b c\n".to_owned(),
        ),
        (
            "second stanza unsigned",
            format!("Types: deb\nURIs: https://a\nSigned-By: {k}\n\nTypes: deb\nURIs: https://b\n"),
        ),
        (
            "second line unsigned",
            format!("deb [signed-by={k}] https://a candidate main\ndeb https://b candidate main\n"),
        ),
        (
            "field name in a comment",
            "Types: deb\nURIs: https://a\n# Signed-By: /x\n".to_owned(),
        ),
        (
            "trusted flag instead",
            "deb [trusted=yes] https://a candidate main\n".to_owned(),
        ),
    ];
    for (what, text) in cases {
        let source = f.write("s.sources", 0o644, &text);
        assert_eq!(f.channel(&source), Err(Refusal::ChannelUnsigned), "{what}");
    }
}

/// A continuation line belongs to the field above it, whatever the field is, and a line
/// that is no field and no entry is ignored: neither can move a key into or out of the
/// `Signed-By` field.
#[test]
fn continuation_and_stray_lines() {
    let f = Fixture::new("config-continuation");
    let key = f.key();
    let text = format!(
        "Types: deb\nURIs: https://a\nSuites: candidate\n stable\nstray words\nSigned-By: {}\nComponents: main\n extra\n",
        key.display()
    );
    let source = f.write("c.sources", 0o644, &text);
    assert!(
        f.channel(&source).is_ok(),
        "continuations of other fields are not keys"
    );
    let text = format!(
        "Types: deb\nURIs: https://a\nSigned-By: {}\n /nonexistent/other.gpg\n",
        key.display()
    );
    let source = f.write("d.sources", 0o644, &text);
    assert_eq!(
        f.channel(&source),
        Err(Refusal::KeyUntrusted),
        "a continuation of Signed-By is a key"
    );
}

/// `path`, written relative to the current directory (`../../tmp/...`).
fn relative_to_cwd(path: &Path) -> String {
    let up = "../".repeat(std::env::current_dir().unwrap().components().count() - 1);
    let relative = format!("{up}{}", path.display().to_string().trim_start_matches('/'));
    assert!(
        Path::new(&relative).is_file(),
        "{relative} must reach the key"
    );
    relative
}

/// Catches a key that is accepted without being root's: a missing file, a file of another
/// owner, a writable file, a key that is not a path (fingerprint, inline), and a list in
/// which only the second key is bad.
#[test]
fn an_untrusted_key_is_refused() {
    let f = Fixture::new("config-key");
    let good = f.key();
    let writable = f.write("writable.gpg", 0o666, "key");
    let missing = f.dir.join("missing.gpg");
    for (what, signed_by) in [
        ("missing key", missing.display().to_string()),
        (
            "group-writable key",
            f.write("gw.gpg", 0o664, "k").display().to_string(),
        ),
        ("world-writable key", writable.display().to_string()),
        (
            "second key bad",
            format!("{}, {}", good.display(), missing.display()),
        ),
        (
            "fingerprint",
            "0123456789ABCDEF0123456789ABCDEF01234567".into(),
        ),
        ("relative path", "keys/kbf.gpg".into()),
        // A relative path that does reach the good key from here is still not a key: sshd's
        // working directory is not the allowlist's.
        ("relative path that exists", relative_to_cwd(&good)),
        ("empty", String::new()),
        (
            "inline armor",
            "-----BEGIN PGP PUBLIC KEY BLOCK-----".into(),
        ),
    ] {
        let source = f.deb822(&signed_by);
        assert_eq!(f.channel(&source), Err(Refusal::KeyUntrusted), "{what}");
    }
    // An inline key on continuation lines is not a path either.
    let inline = "Types: deb\nURIs: https://a\nSigned-By:\n -----BEGIN PGP PUBLIC KEY BLOCK-----\n abc\n -----END PGP PUBLIC KEY BLOCK-----\n";
    let source = f.write("inline.sources", 0o644, inline);
    assert_eq!(
        f.channel(&source),
        Err(Refusal::KeyUntrusted),
        "inline on continuation lines"
    );
    // A key of another owner: a file root owns, judged by a user that is not root. (Run as
    // root, no file here is another user's, so the case is skipped.)
    if f.trust_uid() != 0 {
        let source = f.deb822("/etc/passwd");
        assert_eq!(
            f.channel(&source),
            Err(Refusal::KeyUntrusted),
            "key of another owner"
        );
    }
}

/// Catches a source file nobody checked: a source another user could write is refused as
/// unsafe, before its contents are read.
#[test]
fn a_writable_source_is_unsafe() {
    let f = Fixture::new("config-source-mode");
    let key = f.key();
    let source = f.deb822(&key.display().to_string());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o664)).unwrap();
    assert_eq!(f.channel(&source), Err(Refusal::ConfigUnsafe));
    fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(f.channel(&source).is_ok());
    assert_eq!(
        f.channel(&f.dir.join("nothing.sources")),
        Err(Refusal::ConfigInvalid),
        "missing source"
    );
    let link = f.dir.join("link.sources");
    symlink(&source, &link).unwrap();
    assert_eq!(
        f.channel(&link),
        Err(Refusal::ConfigInvalid),
        "a link is not followed"
    );
}

/// Catches an allowlist that is trusted without being root's: not owned, or writable.
#[test]
fn an_untrusted_allowlist_is_refused() {
    let f = Fixture::new("config-file");
    let text = "[channels.candidate]\nsuite = \"candidate\"\nsource_list = \"/x.sources\"\n";
    let path = f.write("a.toml", 0o644, text);
    assert!(Allowlist::load(&f.trust, &path).is_ok());
    let stranger = Trust::owned_by(f.trust_uid() + 1);
    assert_eq!(
        Allowlist::load(&stranger, &path).unwrap_err(),
        Refusal::ConfigUnsafe
    );
    for mode in [0o664, 0o646, 0o666] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            Allowlist::load(&f.trust, &path).unwrap_err(),
            Refusal::ConfigUnsafe,
            "{mode:o}"
        );
    }
}

/// Catches an allowlist that is guessed at: missing, unparseable, unknown keys, bad names,
/// a relative source path, a bad suite.
#[test]
fn a_bad_allowlist_is_invalid() {
    let f = Fixture::new("config-bad");
    assert_eq!(
        Allowlist::load(&f.trust, &f.dir.join("none.toml")).unwrap_err(),
        Refusal::ConfigInvalid
    );
    for (what, text) in [
        ("not toml", "channels = ["),
        ("no channels table", ""),
        ("unknown top key", "extra = 1\n[channels]\n"),
        (
            "unknown channel key",
            "[channels.c]\nsuite = \"s\"\nsource_list = \"/x\"\nkey = \"/k\"\n",
        ),
        ("missing suite", "[channels.c]\nsource_list = \"/x\"\n"),
        ("missing source_list", "[channels.c]\nsuite = \"s\"\n"),
        (
            "upper-case channel name",
            "[channels.C]\nsuite = \"s\"\nsource_list = \"/x\"\n",
        ),
        (
            "dotted channel name",
            "[channels.\"a.b\"]\nsuite = \"s\"\nsource_list = \"/x\"\n",
        ),
        (
            "path channel name",
            "[channels.\"../x\"]\nsuite = \"s\"\nsource_list = \"/x\"\n",
        ),
        (
            "relative source",
            "[channels.c]\nsuite = \"s\"\nsource_list = \"x.sources\"\n",
        ),
        (
            "suite with a space",
            "[channels.c]\nsuite = \"a b\"\nsource_list = \"/x\"\n",
        ),
        (
            "suite with an option",
            "[channels.c]\nsuite = \"-t\"\nsource_list = \"/x\"\n",
        ),
        (
            "empty suite",
            "[channels.c]\nsuite = \"\"\nsource_list = \"/x\"\n",
        ),
        (
            "source with a newline",
            "[channels.c]\nsuite = \"s\"\nsource_list = \"/x\\ny\"\n",
        ),
    ] {
        let path = f.write("bad.toml", 0o644, text);
        assert_eq!(
            Allowlist::load(&f.trust, &path).unwrap_err(),
            Refusal::ConfigInvalid,
            "{what}"
        );
    }
    let empty = f.write("empty.toml", 0o644, "[channels]\n");
    let list = Allowlist::load(&f.trust, &empty).unwrap();
    assert_eq!(
        list.channel(&f.trust, &Channel::new("candidate").unwrap()),
        Err(Refusal::ChannelNotAllowed)
    );
}

/// Catches a channel that is allowed because it is well formed: only a named channel is
/// usable, and a lookup never reads a file for one that is not named.
#[test]
fn only_a_named_channel_is_allowed() {
    let f = Fixture::new("config-allow");
    let key = f.key();
    let source = f.deb822(&key.display().to_string());
    let list = f.allowlist(&source);
    for name in ["stable", "candidate2", "cand", "a", "0", "x-y"] {
        assert_eq!(
            list.channel(&f.trust, &Channel::new(name).unwrap()),
            Err(Refusal::ChannelNotAllowed),
            "{name}"
        );
    }
}

impl Fixture {
    fn trust_uid(&self) -> u32 {
        fs::metadata(&*self.dir).unwrap().uid()
    }
}
