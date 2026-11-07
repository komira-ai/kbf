//! `kbf-js-stand-in`: the `crun`, `ip`, `nft` and `nsenter` of the executor's real-service
//! test, as the name it is run by says; the programs are `js_stand_in`'s.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    kbf_integration_test::js_stand_in::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {}
