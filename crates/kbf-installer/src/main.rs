//! `kbf-installer`: parse the command line (`kbf_installer::cli`), run the verb
//! (`kbf_installer::app`), exit with the code the contract names (`kbf_installer::exit`).
//! `--version --json` is answered first, from `kbf_installer::version`, before any parse.
//! `deploy`, the gamma trigger (docs/design/gamma-deploy.md), has its own flags and is run
//! before the installer's own command line is parsed.

use std::process::ExitCode;

use clap::Parser;
use kbf_installer::app::dispatch;
use kbf_installer::cli::Cli;
use kbf_installer::version;

fn main() -> ExitCode {
    // `--version --json` is answered before the command line is parsed: it is not a verb's
    // flag (`--version` ends clap's parse by itself, and `--json` is the verbs' global flag).
    let args: Vec<_> = std::env::args_os().collect();
    if version::asked(&args) {
        print!("{}", version::line());
        return ExitCode::SUCCESS;
    }
    if args.get(1).is_some_and(|a| a == "deploy") {
        let rest: Vec<String> = args[2..]
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let code = kbf_installer::deploy::cli::main(
            &rest,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        );
        std::process::exit(code);
    }
    let cli = Cli::parse();
    let euid = rustix::process::geteuid().as_raw();
    dispatch(&cli, euid, &mut std::io::stdout(), &mut std::io::stderr()).into()
}
