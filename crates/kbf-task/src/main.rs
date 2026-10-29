//! `kbf-task`: runs one build action and stores its pack; see the crate documentation for the
//! steps and `kbf_task::task` for the flags and the exit codes.

use clap::Parser as _;

fn main() -> std::process::ExitCode {
    let cli = kbf_task::task::Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("kbf-task: cannot start the async runtime: {e}");
            return std::process::ExitCode::from(kbf_task::task::EXIT_SETUP);
        }
    };
    runtime.block_on(kbf_task::task::main_with(&cli))
}
