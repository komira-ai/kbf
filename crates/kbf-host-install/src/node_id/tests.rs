use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;

use super::read;
use crate::trust::Trust;

struct Fx {
    dir: TestDir,
    trust: Trust,
}

impl Fx {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let trust = Trust::owned_by(fs::metadata(&*dir).unwrap().uid(), &dir);
        Self { dir, trust }
    }

    fn put(&self, name: &str, mode: u32, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn id(&self, config: &str, hostname: Option<&str>) -> Option<String> {
        let config = self.put("kbf.toml", 0o640, config);
        let hostname = hostname.map_or_else(
            || self.dir.join("no-such-hostname"),
            |text| self.put("hostname", 0o644, text),
        );
        read(&self.trust, &config, &hostname)
    }
}

/// An enrolling node names itself under `[node]`; that wins over the other spellings.
#[test]
fn the_node_table_names_the_id() {
    let fx = Fx::new("node-id-node");
    let text = "mode = \"daemon\"\n[node]\nid = \"build-7\"\n[identity]\nnode-id = \"other\"\n";
    assert_eq!(fx.id(text, Some("host\n")), Some("build-7".into()));
}

/// A host that does not enroll (all-in-one) names itself under `[identity]`.
#[test]
fn the_identity_table_names_the_id_of_a_host_that_does_not_enroll() {
    let fx = Fx::new("node-id-identity");
    assert_eq!(
        fx.id("[identity]\nnode-id = \"aio-1\"\n", None),
        Some("aio-1".into())
    );
}

/// With no id in the file the node uses its short host name in lower case, as it does when
/// it enrolls.
#[test]
fn the_short_host_name_is_the_default() {
    let fx = Fx::new("node-id-hostname");
    assert_eq!(
        fx.id("mode = \"daemon\"\n", Some("Build-7.example.org\n")),
        Some("build-7".into())
    );
}

/// Whatever is not a usable id gives none, never a guess: a value that is not a DNS label, a
/// non-string, a host name that is not a label, and no host name file.
#[test]
fn an_unusable_id_is_none() {
    let fx = Fx::new("node-id-bad");
    let too_long = format!("[node]\nid = \"{}\"\n", "a".repeat(64));
    for (what, config, hostname) in [
        ("upper case", "[node]\nid = \"Node\"\n", None),
        ("edge dash", "[node]\nid = \"-a\"\n", None),
        ("trailing dash", "[node]\nid = \"a-\"\n", None),
        ("empty", "[node]\nid = \"\"\n", None),
        ("too long", too_long.as_str(), None),
        ("underscore", "[node]\nid = \"a_b\"\n", None),
        ("not a string", "[node]\nid = 7\n", None),
        ("not toml", "[node\n", None),
        ("host name with a space", "", Some("a b\n")),
        ("no host name", "", None),
    ] {
        assert_eq!(fx.id(config, hostname), None, "{what}");
    }
    let longest = "a".repeat(63);
    assert_eq!(
        fx.id(&format!("[node]\nid = \"{longest}\"\n"), None),
        Some(longest)
    );
    assert_eq!(
        fx.id("[node]\nid = \"0a-b9\"\n", None),
        Some("0a-b9".into())
    );
}

/// A configuration that is missing, or that someone other than the trusted user could have
/// written, gives no id.
#[test]
fn an_untrusted_or_missing_configuration_gives_none() {
    let fx = Fx::new("node-id-trust");
    let hostname = fx.put("hostname", 0o644, "host\n");
    assert_eq!(
        read(&fx.trust, Path::new("/no/such/kbf.toml"), &hostname),
        None
    );
    let writable = fx.put("writable.toml", 0o666, "[node]\nid = \"x\"\n");
    assert_eq!(read(&fx.trust, &writable, &hostname), None);
}
