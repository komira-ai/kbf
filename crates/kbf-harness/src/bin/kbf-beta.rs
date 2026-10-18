//! `kbf-beta`: runs a beta scenario over a farm of real processes and writes
//! `beta-result.json` (`kbf_harness::cli` says how; `kbf_harness::b0` what B0 checks).

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    // SIGTERM and SIGINT (a CI job's cancel or timeout) end the run through its normal exits:
    // the farm stopped, the Buck2 daemons killed, the result document finished as failed.
    kbf_harness::interrupt::install();
    kbf_harness::cli::run(std::env::args_os(), kbf_harness::cli::env_var)
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("kbf-beta runs on Linux only");
    std::process::ExitCode::from(2)
}
