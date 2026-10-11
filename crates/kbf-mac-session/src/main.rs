//! `kbf-mac-session`: see the library's documentation.

use clap::Parser as _;

fn main() -> std::process::ExitCode {
    // The mode `run` in a GUI session executes this binary in (`macos::gui`), as root
    // under `launchctl asuser`; never a flag of the helper itself.
    #[cfg(target_os = "macos")]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        if let Some((mode, rest)) = args.split_first()
            && mode == kbf_mac_session::macos::gui::AS_USER
        {
            return kbf_mac_session::macos::gui::as_user(rest);
        }
    }
    kbf_mac_session::start::main(&kbf_mac_session::start::Args::parse())
}
