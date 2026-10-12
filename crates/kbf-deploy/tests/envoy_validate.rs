//! Envoy's own validation of the rendered bootstrap: the pinned Envoy release binary
//! run as `envoy --mode validate`, which parses the whole config against Envoy's
//! protos and builds every listener, cluster and TLS context without binding a port
//! or resolving a name. `envoy check` reads only the fields it models; this is what
//! refuses a field Envoy does not know.
//!
//! Ignored in the plain test run: the binary is not in the repository. CI's
//! test-x86 job fetches the release `templates/envoy/config.example`'s image runs
//! (`cargo xtask fetch`, pinned by sha256) and runs these with `--ignored`, naming
//! the binary in `KBF_TEST_ENVOY`. Run without it, each test fails rather than
//! passes.
//!
//! The config names the CA at the pod's mount (`render::CA_FILE`), which does not
//! exist here; the copy validated points it at a CA certificate made for the test.
//! Nothing else is changed.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_deploy::envoy::EnvoyConfig;
use kbf_deploy::envoy::render::{self, CA_FILE};

/// The pinned Envoy binary CI fetched.
fn envoy_binary() -> PathBuf {
    let path = std::env::var_os("KBF_TEST_ENVOY")
        .expect("KBF_TEST_ENVOY names the pinned Envoy binary (CI's test-x86 job fetches it)");
    PathBuf::from(path)
}

/// The example's render, its CA path pointed at a fresh test CA under `dir`.
fn example_envoy_json(dir: &Path) -> String {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let c = EnvoyConfig::load(&manifest.join("templates/envoy/config.example")).expect("config");
    let text = render::render(&c).envoy;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().expect("key");
    let ca = params.self_signed(&key).expect("CA");
    let ca_path = dir.join("ca.pem");
    std::fs::write(&ca_path, ca.pem()).expect("write the CA");
    assert!(
        text.matches(CA_FILE).count() >= 2,
        "the CA path {CA_FILE} is not in the config"
    );
    text.replace(CA_FILE, ca_path.to_str().expect("utf-8"))
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

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Catches a field Envoy does not know or a value of the wrong type in the render (a
/// typo `envoy check` reads past, such as `stream_idle_timout`, which Envoy would
/// refuse at pod start, so the front never comes up).
#[test]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
fn envoy_accepts_the_example_render() {
    let dir = scratch("envoy-validate-example");
    let text = example_envoy_json(&dir);
    let (code, out) = validate(&dir, "envoy.json", &text);
    assert_eq!(code, Some(0), "Envoy refuses the render:\n{out}");
}

/// The control. Catches a validation that accepts anything (a binary that is not
/// Envoy, or a mode that does not parse the config), without which the test above
/// proves nothing: the same render with one field misspelt must be refused.
#[test]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
fn envoy_refuses_an_unknown_field() {
    let dir = scratch("envoy-validate-control");
    let text = example_envoy_json(&dir);
    let field = "\"stream_idle_timeout\"";
    assert_eq!(text.matches(field).count(), 1, "the field to misspell");
    let control = text.replace(field, "\"stream_idle_timout\"");
    let (code, out) = validate(&dir, "control.json", &control);
    assert_ne!(code, Some(0), "Envoy accepted stream_idle_timout:\n{out}");
    assert!(
        out.contains("stream_idle_timout"),
        "refused for another reason:\n{out}"
    );
}
