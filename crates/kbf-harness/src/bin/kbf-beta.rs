//! `kbf-beta`: runs a beta scenario over a farm of real processes and writes
//! `beta-result.json` (`kbf_harness::cli` says how; `kbf_harness::b0` what B0 checks).

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    kbf_harness::cli::run(std::env::args_os(), |key| std::env::var(key).ok())
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("kbf-beta runs on Linux only");
    std::process::ExitCode::from(2)
}
