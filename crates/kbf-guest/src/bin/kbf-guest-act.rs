//! `kbf-guest-act`: the act verbs of `kbf_testkit::act`, the program this crate's
//! tests give the agent to run in place of `sh -c`. Test-only: not part of what ships.

fn main() -> std::process::ExitCode {
    kbf_testkit::act::main()
}
