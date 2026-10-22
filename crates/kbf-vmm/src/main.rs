//! `kbf-vmm`: see the library's documentation. On a platform other than macOS every
//! command except `--help` and `--version` refuses.

use std::process::ExitCode;

use clap::Parser;
use kbf_vmm::app;
use kbf_vmm::cli::Cli;
use kbf_vmm::probe::Timing;

fn main() -> ExitCode {
    let cli = Cli::parse();
    #[cfg(target_os = "macos")]
    let real = kbf_vmm::real::RealSystem::new();
    #[cfg(target_os = "macos")]
    let sys: Option<&dyn kbf_vmm::system::System> = Some(&real);
    #[cfg(not(target_os = "macos"))]
    let sys: Option<&dyn kbf_vmm::system::System> = None;
    app::run(
        cli,
        sys,
        Timing::real(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}
