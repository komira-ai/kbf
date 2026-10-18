//! `fetch` against real HTTPS: a local server with its own CA, answering each path with
//! a canned response, and the `cargo xtask fetch` binary for its flags and exit codes.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use xtask::fetch::{self, Sha256, Unpack};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const TOOL: &[u8] = b"a pinned tool";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-fetch-https")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A canned HTTP/1.1 response.
fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in headers {
        r.push_str(&format!("{name}: {value}\r\n"));
    }
    r.push_str("Connection: close\r\n\r\n");
    [r.into_bytes(), body.to_vec()].concat()
}

/// The path of a request read from `stream` (its head, up to the blank line), or `None`
/// when the client went away first (a handshake it refused).
fn request_path(stream: impl std::io::Read) -> Option<String> {
    let mut lines = BufReader::new(stream).lines();
    let first = lines.next()?.ok()?;
    for line in lines {
        if line.ok()?.is_empty() {
            break;
        }
    }
    Some(first.split(' ').nth(1)?.to_owned())
}

/// Serves `routes` (path, response) forever on a loopback port, over TLS with a
/// certificate from a fresh CA when `tls`, else plain: the base URL, and the
/// CA's certificate.
fn serve(
    tls: bool,
    routes: impl Fn(&str, &str) -> Vec<u8> + Send + 'static,
) -> (String, CertificateDer<'static>) {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("params")
        .signed_by(&key, &ca)
        .expect("sign");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("server config");
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let base = format!("{}://127.0.0.1:{port}", if tls { "https" } else { "http" });
    let origin = base.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept");
            if tls {
                let conn = rustls::ServerConnection::new(Arc::clone(&config)).expect("conn");
                let mut tls = rustls::StreamOwned::new(conn, stream);
                // A handshake the client abandons (an untrusted certificate) ends here.
                let Some(path) = request_path(&mut tls) else {
                    continue;
                };
                let _ = tls.write_all(&routes(&origin, &path));
                tls.conn.send_close_notify();
                let _ = tls.flush();
            } else {
                let mut stream = stream;
                let Some(path) = request_path(&mut stream) else {
                    continue;
                };
                let _ = stream.write_all(&routes(&origin, &path));
            }
        }
    });
    (base, ca.der().clone())
}

#[test]
fn a_download_follows_redirects_and_returns_the_body() {
    // Catches: a client that does not follow redirects (every GitHub release download
    // answers 302 to its storage host) or returns the redirect's own body.
    let (base, ca) = serve(true, |origin, path| match path {
        "/release" => response(
            "302 Found",
            &[("Location", &format!("{origin}/blob"))],
            b"go",
        ),
        _ => response("200 OK", &[], TOOL),
    });
    let got = fetch::download(&format!("{base}/release"), &[ca]).expect("download");
    assert_eq!(got, TOOL);
}

#[test]
fn a_status_that_is_not_2xx_is_an_error() {
    // Catches: an error page taken as the download (the hash check would refuse it,
    // with a message that hides the 404).
    let (base, ca) = serve(true, |_, _| {
        response("404 Not Found", &[], b"no such release")
    });
    let url = format!("{base}/missing");
    let Err(fetch::Error::Download { url: got, why }) = fetch::download(&url, &[ca]) else {
        panic!("a 404 was not an error");
    };
    assert_eq!(got, url);
    assert!(why.contains("404"), "{why}");
}

#[test]
fn a_redirect_to_plain_http_is_refused() {
    // Catches: a client that leaves HTTPS on a redirect (the plain server here would
    // answer with the tool).
    let (plain, _) = serve(false, |_, _| response("200 OK", &[], TOOL));
    let (base, ca) = serve(true, move |_, _| {
        response("302 Found", &[("Location", &format!("{plain}/blob"))], b"")
    });
    let result = fetch::download(&format!("{base}/release"), &[ca]);
    let Err(fetch::Error::Download { why, .. }) = result else {
        panic!("{result:?}");
    };
    assert!(why.contains("https"), "{why}");
}

#[test]
fn a_certificate_the_roots_do_not_trust_is_refused() {
    // Catches: certificate checks turned off, or roots other than the ones given: the
    // host's roots do not include this test CA.
    let (base, _) = serve(true, |_, _| response("200 OK", &[], TOOL));
    let roots = fetch::host_roots();
    let result = fetch::download(&format!("{base}/blob"), &roots);
    assert!(
        matches!(result, Err(fetch::Error::Download { .. })),
        "{result:?}"
    );
}

#[test]
fn fetch_installs_a_matching_download_and_refuses_a_mismatch() {
    // Catches: `fetch` installing without the hash check, or reporting success for a
    // file it did not write.
    let (base, ca) = serve(true, |_, _| response("200 OK", &[], TOOL));
    let out = scratch("fetch").join("bin").join("tool");
    let url = format!("{base}/tool");
    let line = fetch::fetch(
        &url,
        &Sha256::of(TOOL),
        &Unpack::Raw,
        &out,
        std::slice::from_ref(&ca),
    )
    .expect("fetch");
    assert_eq!(
        line,
        format!(
            "ok: {url} has SHA-256 {}; installed {}",
            Sha256::of(TOOL),
            out.display()
        )
    );
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    let other = scratch("fetch-mismatch").join("tool");
    let pinned = Sha256::of(b"another tool");
    let result = fetch::fetch(&url, &pinned, &Unpack::Raw, &other, &[ca]);
    assert!(
        matches!(result, Err(fetch::Error::Mismatch { .. })),
        "{result:?}"
    );
    assert!(!other.exists() && !fetch::part_path(&other).exists());
}

/// `cargo xtask fetch` with `args`: its exit code and stderr.
fn xtask_fetch(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(XTASK)
        .arg("fetch")
        .args(args)
        .output()
        .expect("run xtask");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const PIN: &str = "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f";

#[test]
fn the_verb_refuses_plain_http_with_exit_1_and_writes_nothing() {
    // Catches: a verb that downloads over plain HTTP, or exits 0 when it installed
    // nothing (the job would go on without its tool).
    let out = scratch("cli-http").join("tool");
    let url = "http://127.0.0.1:9/tool";
    let (code, err) = xtask_fetch(&[
        "--url",
        url,
        "--sha256",
        PIN,
        "--out",
        out.to_str().expect("utf-8"),
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.starts_with(&format!("xtask: download {url}: ")),
        "{err}"
    );
    assert!(!out.exists() && !fetch::part_path(&out).exists());
}

#[test]
fn flags_that_do_not_fit_together_exit_2() {
    // Catches: a malformed pin, a tar without --member or a --member without a tar
    // accepted (exit 0 or 1 instead of the usage status), before any download.
    let out = scratch("cli-usage").join("tool");
    let out = out.to_str().expect("utf-8");
    let url = "https://127.0.0.1:9/tool";
    for (extra, says) in [
        (&["--sha256", "ABC"][..], "not a SHA-256"),
        (
            &["--sha256", PIN, "--extract", "tar.gz"][..],
            "need --member",
        ),
        (
            &["--sha256", PIN, "--extract", "zst", "--member", "a"][..],
            "needs --extract tar.gz, tar.xz or zip",
        ),
        (&["--sha256", PIN, "--extract", "zip"][..], "need --member"),
        (&["--sha256", PIN, "--extract", "rar"][..], "invalid value"),
    ] {
        let mut args = vec!["--url", url, "--out", out];
        args.extend_from_slice(extra);
        let (code, err) = xtask_fetch(&args);
        assert_eq!(code, Some(2), "{extra:?}: {err}");
        assert!(err.contains(says), "{extra:?}: {err}");
    }
    assert!(!Path::new(out).exists());
}
