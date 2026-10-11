//! `kbf-vmm`: the VM helper for macOS VM leases (docs/design/macos-vm-guests.md).
//!
//! ```text
//! kbf-vmm --version
//! ```
//!
//! A stub. It prints its version for `--version` and refuses everything else with
//! exit status 2, because no VM code exists yet. It is the binary the signing scripts
//! sign with `com.apple.security.virtualization`, the one entitlement their allow list
//! gives anything (tools/ci/darwin-entitlements.txt; design section 7).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args == ["--version"] {
        println!("kbf-vmm {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "kbf-vmm: only --version is implemented; the VM helper is planned \
         (docs/design/macos-vm-guests.md)"
    );
    ExitCode::from(2)
}
