//! TEST-ONLY STUB of the `kbf-installer` binary. The package build and install test
//! (`cargo xtask pkg-build`, `pkg-install-test`) package it until the real binary
//! lands; swapping it in is one path change (the `--binary` of `pkg-config`). Nothing
//! ships it. It prints its arguments and exits 0.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("kbf-installer-stub (test only) {}", args.join(" "));
}
