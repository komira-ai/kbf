//! `kbf-deploy envoy render` as a user runs it: the files it writes are the goldens,
//! byte for byte.

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;

/// Catches a binary whose written files differ from what the library renders (a
/// newline added or lost on the way to disk, a file written under another name), so
/// the bytes the pods hash are the bytes the golden pins.
#[test]
fn the_binary_writes_the_golden_files_for_the_example_config() {
    let own = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "envoy_cli");
    let dir = own.join("out");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-deploy"))
        .args(["envoy", "render", "--config"])
        .arg(manifest.join("templates/envoy/config.example"))
        .arg("--out")
        .arg(&dir)
        .output()
        .expect("run kbf-deploy");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for name in ["manifests.json", "envoy.json"] {
        let got = std::fs::read(dir.join(name)).expect("rendered");
        let want = std::fs::read(manifest.join("tests/golden/envoy/one-server").join(name))
            .expect("golden");
        assert!(got == want, "{name} differs from its golden");
    }
}
