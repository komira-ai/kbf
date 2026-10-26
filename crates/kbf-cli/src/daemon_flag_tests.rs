//! The daemon's flags: it takes no store, credential or environment variable, but for the
//! flags of `--pack-read`.

use super::*;

/// Catches a build node that takes a store or a credential (docs/design/cluster.md,
/// sections 3.3, S2 to S4) through a flag only the daemon gained, which the refusal
/// cannot see, or through the environment: clap's `env` feature, which a flag needs to
/// read a variable, is off in the workspace. The daemon's only credentials are its node
/// identity (`--ca-cert`, `--cert`, `--key`) and, for `--pack-read`, the file of a key pair that
/// may read the farm's packs and write nothing, and, for `--pack-write`, the file of one that
/// may create them. The words are the wire test's
/// (kbf-proto's tests/daemon_wire.rs): a part (`store`) is matched anywhere in a flag, a
/// short word (`sts`, `url`, `key`) only as a whole word between dashes, except in the
/// reviewed flags of `REVIEWED_DAEMON_FLAGS`, each of which must still be a flag, and the
/// nine flags of `PACK_READ_FLAGS`, which name a store, a bucket, an endpoint, a region and
/// a credentials file as a path and are checked for neither. Of all the daemon's flags,
/// only `--pack-store-credentials-file` names a credential, and it takes a `PATH`.
#[test]
fn the_daemon_takes_no_store_credential_or_environment_variable() {
    /// Flags that hold a short word and were reviewed.
    const REVIEWED_DAEMON_FLAGS: &[&str] = &[
        // The node identity's private key.
        "key",
        // The socket `kbf-mac-session` serves: a macOS login session, not a cloud one.
        "mac-session-socket",
    ];
    /// The flags of `--pack-read` (`kbf_node`'s `pack_read`): the object store the daemon
    /// reads packs from and writes them to, and the files of its key pairs, never a key itself.
    const PACK_READ_FLAGS: &[&str] = &[
        "pack-read",
        "pack-read-cache-bytes",
        "pack-read-cache-dir",
        "pack-store-bucket",
        "pack-store-credentials-file",
        "pack-store-endpoint",
        "pack-store-region",
        "pack-store-write-credentials-file",
        "pack-write",
    ];
    let flags = longs(&kbf_node::Cli::command());
    for flag in &flags {
        if PACK_READ_FLAGS.contains(&flag.as_str()) {
            continue;
        }
        for part in [
            "store",
            "cold",
            "bucket",
            "endpoint",
            "credential",
            "secret",
            "access-key",
            "password",
            "region",
            "presign",
            "auth",
            "tls-cert",
            "tls-key",
        ] {
            assert!(!flag.contains(part), "--{flag} names {part}");
        }
        if REVIEWED_DAEMON_FLAGS.contains(&flag.as_str()) {
            continue;
        }
        for word in [
            "s3", "aws", "sts", "session", "token", "url", "uri", "key", "object",
        ] {
            let plural = format!("{word}s");
            assert!(
                !flag.split('-').any(|w| w == word || w == plural),
                "--{flag} names {word}"
            );
        }
    }
    for identity in ["ca-cert", "cert", "key"] {
        assert!(flags.contains(identity), "{identity}");
    }
    for reviewed in REVIEWED_DAEMON_FLAGS.iter().chain(PACK_READ_FLAGS) {
        assert!(
            flags.contains(*reviewed),
            "--{reviewed} is no longer a flag"
        );
    }
    // The key pairs are files: the only flags that name a credential name one as a path.
    let named: Vec<&String> = flags.iter().filter(|f| f.contains("credential")).collect();
    let files = [
        "pack-store-credentials-file",
        "pack-store-write-credentials-file",
    ];
    assert_eq!(named, files);
    let command = kbf_node::Cli::command();
    for name in files {
        let file = command
            .get_arguments()
            .find(|arg| arg.get_long() == Some(name))
            .expect("the credentials file flag");
        let values = file.get_value_names().unwrap_or_default();
        let shown: Vec<String> = values.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["PATH"], "--{name}");
    }
    let workspace = include_str!("../../../Cargo.toml");
    let clap = workspace
        .lines()
        .find(|l| l.starts_with("clap = "))
        .expect("the workspace names clap");
    assert!(!clap.contains("\"env\""), "{clap}");
}
