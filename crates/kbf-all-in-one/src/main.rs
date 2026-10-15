//! `kbf-all-in-one`: the farm server and a worker daemon in one process. The binary is
//! [`kbf_all_in_one::main`]; the library's docs say what it does.

use std::process::ExitCode;

fn main() -> ExitCode {
    kbf_all_in_one::main()
}
