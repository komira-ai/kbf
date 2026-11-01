//! `kbf-act`: the act verbs and the argv0 fakes (`kbf_testkit::act`). It answers through
//! [`kbf_testkit::act::hook`] first, as a test binary that expands `act_test_hook!` does;
//! run under another name, it answers through `act::main`, as another crate's bin does.

fn main() -> std::process::ExitCode {
    kbf_testkit::act::hook();
    kbf_testkit::act::main()
}
