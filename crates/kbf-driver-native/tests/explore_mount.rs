//! TEMPORARY exploration of komira-build-farm#178 (not to be merged).
#![cfg(target_os = "macos")]

mod support;

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::Runtime;
use support::{MemoryCas, Spec, config, runtime, scratch, stderr, stdout, work};

fn sh(label: &str, script: &str) {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .output()
        .expect("sh");
    eprintln!(
        "=== {label}: {}\n{}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn explore() {
    sh(
        "versions",
        "sw_vers; xcode-select -p; ls /Applications | grep -i xcode; xcodebuild -version",
    );
    sh(
        "mounts before",
        "mount | grep -i -E 'metal|DVT|cryptex' | head -20",
    );
    let dir = scratch("explore");
    let cas = Arc::new(MemoryCas::default());
    let cfg = config(&dir);
    let rt = runtime(cfg, &cas);
    let script = r#"
export CFFIXED_USER_HOME="$HOME"
echo HOME=$HOME
mkdir -p Sources/hello && mv main.swift Sources/hello/
echo "--- showComponent"; xcodebuild -showComponent MetalToolchain 2>&1 | tail -5
echo "--- mounts after showComponent"; mount | grep -i -E 'metal|DVT' | head
echo "--- build"; xcodebuild -scheme hello -destination 'platform=macOS' -derivedDataPath "$TMPDIR/dd" build 2>&1 | tail -15
echo "--- mounts after build"; mount | grep -i -E 'metal|DVT' | head
find "$HOME/Library" -maxdepth 6 2>/dev/null | head -40
"#;
    let mut spec = Spec::sh(script).env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    spec.inputs = vec![
        (
            "Package.swift".to_owned(),
            b"// swift-tools-version:5.9\nimport PackageDescription\nlet package = Package(name: \"hello\", targets: [.executableTarget(name: \"hello\", path: \"Sources/hello\")])\n".to_vec(),
        ),
        ("main.swift".to_owned(), b"print(\"hello\")\n".to_vec()),
    ];
    let action = spec.timeout(Duration::from_secs(900)).store(&cas);
    let ran = tokio::time::timeout(Duration::from_secs(930), rt.run(work(1, action, 0)))
        .await
        .expect("ends");
    match &ran {
        Ok(r) => eprintln!(
            "RAN exit={}\n{}\n{}",
            r.exit_code,
            stdout(&cas, r),
            stderr(&cas, r)
        ),
        Err(e) => eprintln!("RUN ERR {e:?}"),
    }
    sh("mounts after run", "mount | grep -i -E 'metal|DVT' | head");
    sh(
        "leases dir",
        &format!("find '{}' -maxdepth 2 | head -30", dir.display()),
    );
    sh(
        "unmount tries",
        "for mp in $(mount | grep -i -E 'MetalToolchain' | sed -E 's/.* on (.*) \\(.*/\\1/'); do echo \"mp=$mp\"; ls -ld \"$mp\"; diskutil unmount force \"$mp\" 2>&1 | tail -2; umount -f \"$mp\" 2>&1 | tail -2; hdiutil detach -force \"$mp\" 2>&1 | tail -2; done",
    );
    sh(
        "mounts after unmount tries",
        "mount | grep -i -E 'metal|DVT' | head",
    );
    sh(
        "log: mount-ish",
        "log show --last 30m --style compact --predicate 'eventMessage CONTAINS[c] \"MetalToolchain\" OR eventMessage CONTAINS[c] \"DVTDownloads\"' 2>&1 | tail -40 | cut -c1-300",
    );
    // A direct (no driver) run under a sandbox trace, to learn which mach services it uses.
    let home = dir.join("direct-home");
    std::fs::create_dir_all(&home).expect("home");
    let trace = dir.join("trace.sb");
    let pf = dir.join("profile.sb");
    std::fs::write(
        &pf,
        format!(
            "(version 1)\n(trace \"{}\")\n(allow default)\n",
            trace.display()
        ),
    )
    .expect("profile");
    let inner = dir.join("direct.sh");
    std::fs::write(
        &inner,
        format!(
            "export CFFIXED_USER_HOME='{h}'\ncd '{h}'\nmkdir -p Sources/hello\nprintf 'print(1)\\n' > Sources/hello/main.swift\nprintf '// swift-tools-version:5.9\\nimport PackageDescription\\nlet package = Package(name: \"hello\", targets: [.executableTarget(name: \"hello\", path: \"Sources/hello\")])\\n' > Package.swift\nxcodebuild -scheme hello -destination 'platform=macOS' -derivedDataPath dd build 2>&1 | tail -5\n",
            h = home.display()
        ),
    )
    .expect("script");
    sh(
        "direct traced",
        &format!(
            "/usr/bin/sandbox-exec -f '{}' /bin/sh '{}'",
            pf.display(),
            inner.display()
        ),
    );
    sh(
        "trace mach+mount",
        &format!(
            "grep -i -E 'mach-lookup|mount|diskimage|mobile|asset' '{}' | sort -u | head -80",
            trace.display()
        ),
    );
    sh(
        "mounts after direct",
        "mount | grep -i -E 'metal|DVT' | head",
    );
}
