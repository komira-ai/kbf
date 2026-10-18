//! Envoy's own validation of the installer's box render: the pinned Envoy release binary
//! run as `envoy --mode validate`, which parses the whole config against Envoy's protos
//! and builds every listener, cluster and TLS context without binding a port or
//! resolving a name. The box check reads only the fields it models; this is what refuses
//! a field Envoy does not know.
//!
//! Ignored in the plain test run: the binary is not in the repository. CI's test-x86 job
//! fetches the pinned release (`cargo xtask fetch`, sha256) and runs these with
//! `--ignored`, naming the binary in `KBF_TEST_ENVOY`. Run without it, each test fails
//! rather than passes.
//!
//! The render names its three TLS files at the host's paths, which do not exist here;
//! the copy validated points them at files made for the test. Nothing else is changed.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_installer::envoy::{BoxSettings, check, render};
use kbf_it::pki;

/// The pinned Envoy binary CI fetched.
fn envoy_binary() -> PathBuf {
    let path = std::env::var_os("KBF_TEST_ENVOY")
        .expect("KBF_TEST_ENVOY names the pinned Envoy binary (CI's test-x86 job fetches it)");
    PathBuf::from(path)
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A one-host box: the ports and limits of the installer's golden, the TLS files made
/// for this test under `dir`.
fn settings(dir: &Path) -> BoxSettings {
    let files = pki::write(dir, "envoy-validate").expect("certificates");
    let path = |p: &Path| p.to_str().expect("utf-8").to_owned();
    BoxSettings {
        host: pki::SERVER_NAME.to_owned(),
        reapi_port: 8980,
        worker_port: 8981,
        admin_port: 9901,
        backend_port_base: 18980,
        cert_file: path(&files.server_cert),
        key_file: path(&files.server_key),
        ca_file: path(&files.ca_cert),
        reapi_max_streams: 10_000,
        stream_ceiling: 65_536,
        longest_action_secs: 21_600,
    }
}

/// `envoy --mode validate` on `text`: its exit status and output.
fn validate(dir: &Path, name: &str, text: &str) -> (Option<i32>, String) {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write the config");
    let out = Command::new(envoy_binary())
        .args([
            "--mode",
            "validate",
            "--disable-hot-restart",
            "--log-level",
            "warn",
        ])
        .arg("--config-path")
        .arg(&path)
        .output()
        .expect("run envoy");
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    (out.status.code(), text.into_owned())
}

/// Catches a box render Envoy refuses: a field it does not know, a value of the wrong
/// type, a cluster or listener it cannot build (a retry policy, a breaker, a health check
/// of an endpoint's own port, a tcp_proxy idle timeout, or the REAPI listener's TLS
/// context), which the box check reads past. The render is one the check passes too, so
/// the two agree on the same bytes.
#[test]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
fn envoy_accepts_the_box_render() {
    let dir = scratch("envoy-validate-box");
    let settings = settings(&dir);
    let rendered = render(&settings).expect("render");
    assert_eq!(check(&settings, &rendered).expect("check"), vec![]);
    let (code, out) = validate(&dir, "envoy.json", &rendered.envoy);
    assert_eq!(code, Some(0), "Envoy refuses the box render:\n{out}");
}

/// The control for the test above: the same render with one box-only field misspelt must
/// be refused, so a validation that reads nothing of the box's own settings cannot pass
/// it.
#[test]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
fn envoy_refuses_an_unknown_box_field() {
    let dir = scratch("envoy-validate-box-control");
    let text = render(&settings(&dir)).expect("render").envoy;
    let field = "\"close_connections_on_host_health_failure\"";
    assert_eq!(text.matches(field).count(), 2, "the field to misspell");
    let control = text.replace(field, "\"close_connections_on_host_health_failur\"");
    let (code, out) = validate(&dir, "control.json", &control);
    assert_ne!(code, Some(0), "Envoy accepted the misspelt field:\n{out}");
    assert!(
        out.contains("close_connections_on_host_health_failur"),
        "refused for another reason:\n{out}"
    );
}
