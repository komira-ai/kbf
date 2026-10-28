//! A service that runs as a container: what its descriptor holds, and every value it refuses.

use super::{Descriptor, Entry};

const BASE: &str = r#"
kind = "container"
artifact = "js-server"
tree = "js-server"
[container]
name = "js-server"
restart = "unless-stopped"
user = "65532:65532"
ready_url = "http://127.0.0.1:8080/readyz"
"#;

fn entry(text: &str) -> Entry {
    toml::from_str(text).unwrap()
}

fn describe(text: &str) -> Result<Descriptor, String> {
    Descriptor::from_entry("js-server", &entry(text))
}

/// `BASE` with its line `from` (a prefix of one line) replaced by `to` (or removed when empty).
fn with(from: &str, to: &str) -> String {
    let mut out = String::new();
    let mut found = false;
    for line in BASE.lines() {
        if line.starts_with(from) {
            found = true;
            if !to.is_empty() {
                out.push_str(to);
                out.push('\n');
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    assert!(found, "{from}");
    out
}

/// `BASE` with `extra` lines added to its container table.
fn plus(extra: &str) -> String {
    format!("{BASE}{extra}\n")
}

/// A container table makes a descriptor with no unit and no separate program, whose
/// configuration is the frontend's kind of record: the run keys, the loopback address, the
/// probe paths and the images' repository.
#[test]
fn a_container_table_makes_a_descriptor_with_a_run_configuration() {
    let text = plus(
        r#"network = "bridge"
group_add = ["990"]
read_only = true
tmpfs = ["/tmp:rw,size=64m"]
cap_drop = ["ALL"]
security_opt = ["no-new-privileges"]
ulimits = ["nofile=1024:2048"]
ports = ["127.0.0.1:8080:8080"]
mounts = ["/etc/js:/etc/js:ro"]
command = ["--config", "/etc/js/server.toml"]
version_url = "http://127.0.0.1:8080/version"
drain_token_file = "/etc/js/operator.token"
drain_secs = 60
ready_secs = 30"#,
    );
    let d = describe(&text).unwrap();
    assert_eq!(d.name.as_str(), "js-server");
    assert_eq!(
        (d.artifact.as_str(), d.binary.as_str()),
        ("js-server", "js-server")
    );
    assert_eq!(d.tree, "js-server");
    assert!(d.units.is_empty());
    let c = d.container.unwrap();
    assert_eq!(c.run.name, "js-server");
    assert_eq!(c.run.group_add, [990]);
    assert!(c.run.read_only);
    assert_eq!(c.run.command, ["--config", "/etc/js/server.toml"]);
    assert_eq!(c.api, "127.0.0.1:8080".parse().unwrap());
    assert_eq!(c.drain_token_file, "/etc/js/operator.token");
    assert_eq!((c.drain_secs, c.ready_secs), (60, 30));
    assert!(c.peers.is_empty());
    let service = c.service.unwrap();
    assert_eq!(service.repository, "js-server");
    assert_eq!(service.ready_path, "/readyz");
    assert_eq!(service.version_path.as_deref(), Some("/version"));
    assert!(service.drain);
}

/// Without a drain token file there is no drain (and no drain wait), without a version URL
/// there is no version check, and the readiness wait has the frontend's default.
#[test]
fn a_container_without_a_drain_or_a_version_check_has_neither() {
    let c = describe(BASE).unwrap().container.unwrap();
    let service = c.service.unwrap();
    assert!(!service.drain);
    assert_eq!(service.version_path, None);
    assert_eq!((c.drain_secs, c.ready_secs), (0, 90));
    assert_eq!(c.drain_token_file, "");
    // An IPv6 loopback address is loopback too.
    let v6 = with("ready_url", "ready_url = \"http://[::1]:9/ready\"");
    let c = describe(&v6).unwrap().container.unwrap();
    assert_eq!(c.api, "[::1]:9".parse().unwrap());
}

/// What a container needs is stated, and what a service of units has is not allowed on it.
#[test]
fn the_shape_of_a_container_table_is_checked() {
    let refused = |text: &str, word: &str| {
        let err = describe(text).unwrap_err();
        assert!(err.contains(word), "{err}");
    };
    // The container table is required, and only for a container.
    refused(
        "kind = \"container\"\nartifact = \"js-server\"\ntree = \"js-server\"\n",
        "container table",
    );
    refused(&BASE.replace("kind = \"container\"\n", ""), "needs kind");
    refused(
        &BASE.replace(
            "tree = \"js-server\"",
            "tree = \"js-server\"\nbinary = \"x\"",
        ),
        "no binary and no units",
    );
    refused(
        &BASE.replace(
            "tree = \"js-server\"",
            "tree = \"js-server\"\nunits = [\"js-server.service\"]",
        ),
        "no binary and no units",
    );
    // The probe is required, and the table has no key this build does not know.
    assert!(toml::from_str::<Entry>(&with("ready_url", "")).is_err());
    assert!(toml::from_str::<Entry>(&plus("surprise = 1")).is_err());
    // The service's own checks come first: the artifact and the tree.
    refused(&with("artifact", "artifact = \"-x\""), "artifact");
    refused(&with("tree", "tree = \"other\""), "tree");
}

/// The container is the service's: its name is the service's or starts with it and a dash.
#[test]
fn a_container_cannot_take_another_containers_name() {
    for name in ["kbf-frontend", "js-serverx", "js", "other", "js-server_"] {
        let err = describe(&with("name", &format!("name = {name:?}"))).unwrap_err();
        assert!(err.contains("container name"), "{name}: {err}");
    }
    assert!(describe(&with("name", "name = \"js-server-a\"")).is_ok());
}

/// A service runs as a user that is given, and not as root; the host's network and the
/// container runtime's own files are not offered to it.
#[test]
fn values_that_raise_privilege_are_refused() {
    let refused = |text: String, word: &str| {
        let err = describe(&text).unwrap_err();
        assert!(err.contains(word), "{err}");
    };
    refused(with("user", ""), "user is required");
    for user in ["root", "0", "root:root", "65532:0", "0:65532", "65532:root"] {
        refused(with("user", &format!("user = {user:?}")), "root");
    }
    for user in ["1000", "app:app", "65532:65532"] {
        assert!(
            describe(&with("user", &format!("user = {user:?}"))).is_ok(),
            "{user}"
        );
    }
    refused(plus("network = \"host\""), "host");
    assert!(describe(&plus("network = \"bridge\"")).is_ok());
    for source in [
        "/",
        "//",
        "/run",
        "/var/run",
        "/var",
        "/proc",
        "/proc/1",
        "/sys",
        "/sys/kernel",
        "/dev",
        "/dev/mem",
        "/run/docker.sock",
        "/var/run/docker.sock",
        "/run/containerd/containerd.sock",
        "/run/docker",
        "/var/run/docker",
        "/var/lib/docker",
        "/var/lib/docker/volumes",
        "/var/lib/containerd",
        "/etc/js/../../run/docker.sock",
    ] {
        refused(plus(&format!("mounts = [\"{source}:/x\"]")), "mount");
    }
    for source in ["/etc/js", "/var/lib/js", "/run/js", "/var/lib/dockerish"] {
        assert!(
            describe(&plus(&format!("mounts = [\"{source}:/x:ro\"]"))).is_ok(),
            "{source}"
        );
    }
}

/// The run keys are held to the frontend's rules: a value that loosens the container, or is a
/// shell word, is refused with the frontend's words.
#[test]
fn the_run_keys_are_as_strict_as_the_frontends() {
    for (extra, word) in [
        ("restart = \"no\"", "restart"),
        ("security_opt = [\"seccomp=unconfined\"]", "security_opt"),
        ("cap_drop = [\"all\"]", "cap_drop"),
        ("tmpfs = [\"/tmp:exec\"]", "tmpfs"),
        ("ulimits = [\"nofile=2:1\"]", "ulimits"),
        ("group_add = [\"0\"]", "group_add"),
        ("ports = [\"\"]", "port"),
        ("mounts = [\"etc:/etc\"]", "mount"),
        ("command = [\"a\\u0000b\"]", "NUL"),
    ] {
        // These keys are in the table already or are new; a repeated key is a parse error, so
        // the base's own lines are replaced.
        let text = if extra.starts_with("restart") {
            with("restart", extra)
        } else {
            plus(extra)
        };
        let err = describe(&text).unwrap_err();
        assert!(err.contains(word), "{extra}: {err}");
    }
}

/// The probe is on this host: an `http://` URL of a loopback address and a port, with a plain
/// path and nothing else; the version is asked of the same address.
#[test]
fn the_probe_urls_are_loopback_and_plain() {
    let refused = |text: String, word: &str| {
        let err = describe(&text).unwrap_err();
        assert!(err.contains(word), "{err}");
    };
    for url in [
        "https://127.0.0.1:8080/readyz",
        "127.0.0.1:8080/readyz",
        "ftp://127.0.0.1/readyz",
    ] {
        refused(
            with("ready_url", &format!("ready_url = {url:?}")),
            "http://",
        );
    }
    refused(
        with("ready_url", "ready_url = \"http://127.0.0.1:8080\""),
        "no path",
    );
    for url in [
        "http://localhost:8080/readyz",
        "http://127.0.0.1/readyz",
        "http://user@127.0.0.1:8080/readyz",
    ] {
        refused(
            with("ready_url", &format!("ready_url = {url:?}")),
            "IP address and a port",
        );
    }
    for url in [
        "http://10.0.0.5:8080/readyz",
        "http://0.0.0.0:8080/readyz",
        "http://[2001:db8::1]:80/readyz",
    ] {
        refused(
            with("ready_url", &format!("ready_url = {url:?}")),
            "loopback",
        );
    }
    for path in ["/a?b=1", "/a#b", "/a b", "/../x", "/a/../b", "/%2e"] {
        refused(
            with(
                "ready_url",
                &format!("ready_url = \"http://127.0.0.1:8080{path}\""),
            ),
            "path",
        );
    }
    refused(
        with(
            "ready_url",
            &format!("ready_url = \"http://127.0.0.1:8080/{}\"", "a".repeat(129)),
        ),
        "path",
    );
    refused(
        plus("version_url = \"http://127.0.0.1:9090/version\""),
        "address of ready_url",
    );
    refused(
        plus("version_url = \"http://127.0.0.2:8080/version\""),
        "address of ready_url",
    );
    refused(
        plus("version_url = \"http://10.0.0.1:8080/version\""),
        "loopback",
    );
}

/// The drain is the operator drain of a file inside the container, with its own wait; a wait
/// without a drain, a relative file and waits out of range are refused.
#[test]
fn the_drain_and_the_waits_are_checked() {
    let refused = |text: String, word: &str| {
        let err = describe(&text).unwrap_err();
        assert!(err.contains(word), "{err}");
    };
    refused(plus("drain_secs = 10"), "needs drain_token_file");
    refused(plus("drain_token_file = \"operator.token\""), "absolute");
    refused(plus("drain_token_file = \"/a\\u0007b\""), "absolute");
    refused(
        plus("drain_token_file = \"/t\"\ndrain_secs = 1801"),
        "drain_secs",
    );
    refused(plus("ready_secs = 9"), "ready_secs");
    refused(plus("ready_secs = 601"), "ready_secs");
    let d = describe(&plus("drain_token_file = \"/t\"")).unwrap();
    assert_eq!(d.container.unwrap().drain_secs, 180);
}

/// A service of units is as it was, and says so: it has no container, and a container table
/// on it is refused.
#[test]
fn a_service_of_units_has_no_container() {
    let agent: Entry = toml::from_str(
        "artifact = \"js-agent\"\nbinary = \"js-agent\"\ntree = \"js-agent\"\nunits = [\"js-agent.service\"]\n",
    )
    .unwrap();
    assert!(
        Descriptor::from_entry("js-agent", &agent)
            .unwrap()
            .container
            .is_none()
    );
    let explicit: Entry = toml::from_str(
        "kind = \"process\"\nartifact = \"js-agent\"\nbinary = \"js-agent\"\ntree = \"js-agent\"\nunits = [\"js-agent.service\"]\n",
    )
    .unwrap();
    assert!(Descriptor::from_entry("js-agent", &explicit).is_ok());
}
