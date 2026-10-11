use std::io;

use super::*;
use crate::step::UrlError;

/// Catches a name check that lets the kit touch a bucket, policy or account that is not
/// kbf's (no `kbf-` prefix), or one S3 or Kubernetes would refuse later: upper case,
/// `_`, a leading or trailing `-`, or too long.
#[test]
fn names_are_kbf_or_kubernetes_labels() {
    let long = format!("kbf-{}", "a".repeat(59));
    for ok in ["kbf-cas", "kbf-a", "kbf-0-9", long.as_str()] {
        assert_eq!(KbfName::new(ok).map(|n| n.to_string()), Ok(ok.to_owned()));
    }
    let too_long = format!("{long}a");
    for bad in [
        "cas",
        "kbf-",
        "kbf-Cas",
        "kbf-a_b",
        "kbf--a",
        "kbf-a-",
        "kbf-a.b",
        too_long.as_str(),
    ] {
        assert_eq!(KbfName::new(bad), Err(ArgError::KbfName(bad.to_owned())));
    }
    let max = "m".repeat(56);
    for ok in ["minio", "minio-hdd", "a", "9", max.as_str()] {
        assert_eq!(K8sName::new(ok).map(|n| n.to_string()), Ok(ok.to_owned()));
    }
    let over = "m".repeat(57);
    for bad in ["", "-minio", "minio-", "Minio", "mi_nio", over.as_str()] {
        assert_eq!(K8sName::new(bad), Err(ArgError::K8sName(bad.to_owned())));
    }
}

/// Catches an endpoint that carries a path (the kit appends its own) or a user part
/// (a second, unchecked place for a key), and a TLS URL the health check cannot speak.
#[test]
fn an_endpoint_is_http_host_and_port_only() {
    for ok in [
        "http://127.0.0.1:9000",
        "http://127.0.0.1:9000/",
        "http://[::1]:9000",
    ] {
        assert!(endpoint(ok).is_ok(), "{ok}");
    }
    for bad in ["http://127.0.0.1:9000/minio", "http://key@127.0.0.1:9000"] {
        assert_eq!(endpoint(bad), Err(ArgError::Endpoint(bad.to_owned())));
    }
    assert_eq!(
        endpoint("https://h:9000"),
        Err(ArgError::Url(UrlError::Scheme("https://h:9000".to_owned())))
    );
}

/// Catches a key that breaks the `MC_HOST_` URL: a `:`, `@` or `/` in it left as is
/// would move where the user part ends.
#[test]
fn url_encode_keeps_only_unreserved_bytes() {
    assert_eq!(url_encode("aZ09-._~"), "aZ09-._~");
    assert_eq!(url_encode("p@ss:w/rd é"), "p%40ss%3Aw%2Frd%20%C3%A9");
}

/// Catches a refusal whose message does not say what to fix; each is what the operator
/// reads before an apply.
#[test]
fn every_refusal_names_its_cause() {
    let p = |s: &str| HostPath::new(s).expect("path");
    let json = serde_json::from_str::<u8>("x").expect_err("not JSON");
    let cases: Vec<(MinioError, &str)> = vec![
        (
            MinioError::NoAdmin("MINIO_ROOT_USER"),
            "MINIO_ROOT_USER is not set in the environment (the admin key pair is never \
             read from a flag or a file)",
        ),
        (
            MinioError::NotLive {
                url: "http://h/minio/health/live".to_owned(),
                why: "it answered 503".to_owned(),
            },
            "http://h/minio/health/live is not live: it answered 503",
        ),
        (
            MinioError::NotMc(p("/usr/bin/mc")),
            "/usr/bin/mc is not the MinIO client: its --version names no RELEASE",
        ),
        (
            MinioError::InGitTree {
                path: p("/srv/repo/s3.env"),
                git: p("/srv/repo/.git"),
            },
            "/srv/repo/s3.env is inside a git work tree (/srv/repo/.git exists): a key \
             file never goes into a checkout",
        ),
        (
            MinioError::NotAFile(p("/etc/kbf")),
            "/etc/kbf is not a regular file",
        ),
        (
            MinioError::KeyFile {
                path: p("/etc/kbf/s3.env"),
                source: ConfigError::Missing("AWS_SECRET_ACCESS_KEY".to_owned()),
            },
            "/etc/kbf/s3.env: AWS_SECRET_ACCESS_KEY is not set (config file)",
        ),
        (
            MinioError::NoUser("kbf-server".to_owned()),
            "no user kbf-server on this host (id -u, id -g)",
        ),
        (
            MinioError::KeyCannotList {
                path: p("/etc/kbf/s3.env"),
                bucket: KbfName::new("kbf-cas").expect("name"),
            },
            "the key in /etc/kbf/s3.env cannot list bucket kbf-cas",
        ),
        (
            MinioError::Inspect {
                path: p("/etc/kbf/s3.env"),
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            },
            "cannot read /etc/kbf/s3.env: permission denied",
        ),
        (
            MinioError::Query {
                cmd: "/usr/bin/mc ls".to_owned(),
                source: io::Error::other("exit 1"),
            },
            "/usr/bin/mc ls: exit 1",
        ),
        (
            MinioError::Json {
                cmd: "/usr/bin/kubectl get".to_owned(),
                source: json,
            },
            "/usr/bin/kubectl get printed no JSON this kit reads: expected value at line 1 \
             column 1",
        ),
        (
            MinioError::Serialize(serde_json::from_str::<u8>("x").expect_err("not JSON")),
            "cannot write JSON: expected value at line 1 column 1",
        ),
        (
            MinioError::NoS3Port("minio/minio".to_owned()),
            "Service minio/minio has no port named s3 at port 9000",
        ),
        (
            MinioError::NoSelector("minio/minio".to_owned()),
            "Service minio/minio has no selector",
        ),
        (
            MinioError::Held {
                port: 30900,
                by: "a/b".to_owned(),
            },
            "nodePort 30900 is held by a/b: pick another with --node-port",
        ),
        (
            MinioError::NotOurs("minio/minio-kbf-np".to_owned()),
            "Service minio/minio-kbf-np does not carry komira.dev/purpose=kbf-pilot: left \
             alone",
        ),
        (
            MinioError::Scratch(io::Error::from(io::ErrorKind::PermissionDenied)),
            "cannot make a scratch directory: permission denied",
        ),
        (
            MinioError::Output(io::Error::from(io::ErrorKind::BrokenPipe)),
            "cannot write the plan out: broken pipe",
        ),
        (
            MinioError::Path(PathError::Empty("/".to_owned())),
            "\"/\" names no file or directory",
        ),
        (
            MinioError::Url(UrlError::Scheme("h".to_owned())),
            "\"h\" is not an http:// URL (no TLS yet)",
        ),
        (
            MinioError::Deploy(DeployError::IsRoot),
            "this step runs as a user, not as root: run it without sudo",
        ),
    ];
    for (err, want) in cases {
        assert_eq!(err.to_string(), want);
    }
    let arg = [
        (
            ArgError::KbfName("x".to_owned()),
            "\"x\": name it kbf-<lowercase letters, digits and ->, 63 characters at most",
        ),
        (
            ArgError::K8sName("X".to_owned()),
            "\"X\" is not a Kubernetes name (lowercase letters, digits and -, 56 characters at most)",
        ),
        (
            ArgError::Endpoint("http://h/p".to_owned()),
            "\"http://h/p\" is not http://host[:port]: no path and no user",
        ),
        (
            ArgError::Url(UrlError::Port("http://h:0".to_owned())),
            "\"http://h:0\" has a port that is not a number from 1 to 65535",
        ),
    ];
    for (err, want) in arg {
        assert_eq!(err.to_string(), want);
    }
}
