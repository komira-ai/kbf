//! Builds `init/kbf_init.rs`, the first process of every action's container, and leaves
//! it in `OUT_DIR` for the driver to embed (`src/oci/init.rs`).
//!
//! It is one file of the standard library alone, compiled here with `rustc` and linked
//! statically (`-C target-feature=+crt-static`, as CI links `kbf-m2-act`), so that it
//! runs in any image whatever the image's libc, and so that the daemon needs no second
//! file shipped beside it. A static link needs the C library's static archive
//! (`libc.a`: Debian's `libc6-dev`, Fedora's `glibc-static`). Only Linux runs
//! containers: on any other target the embedded file is empty and the OCI backend
//! refuses to start with it.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let source = PathBuf::from("init/kbf_init.rs");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let dest = out.join("kbf-init");
    println!("cargo:rustc-env=KBF_INIT_BINARY={}", dest.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        std::fs::write(&dest, b"").expect("write the empty init");
        return;
    }
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let target = std::env::var("TARGET").expect("TARGET");
    let built = Command::new(rustc)
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "bin",
            "--crate-name",
            "kbf_init",
        ])
        .args(["--target", &target])
        .args(["-C", "target-feature=+crt-static", "-C", "panic=abort"])
        .args([
            "-C",
            "opt-level=s",
            "-C",
            "strip=symbols",
            "-C",
            "debuginfo=0",
        ])
        .arg("-o")
        .arg(&dest)
        .arg(&source)
        .output()
        .expect("run rustc");
    assert!(
        built.status.success(),
        "kbf-init did not build (it is linked statically: the C library's static \
         archive, libc.a, must be installed):\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}
