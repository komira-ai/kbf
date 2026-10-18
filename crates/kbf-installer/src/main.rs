//! `kbf-installer`. Today it has one verb, `deploy`; the rest of the installer's verbs
//! (docs/design/kbf-installer.md) are libraries here and are not wired to a command line.

use std::io::{stderr, stdin, stdout};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("deploy") => kbf_installer::deploy::cli::main(
            &args[1..],
            &mut stdin().lock(),
            &mut stdout().lock(),
            &mut stderr().lock(),
        ),
        _ => {
            eprintln!("usage: kbf-installer deploy ...  (the only verb built so far)");
            2
        }
    };
    std::process::exit(code);
}
