//! Writes out the sandbox the native driver runs an action that asks for the network
//! under, so a probe can run a program in it without the daemon: the XCUITest spike
//! (`tools/spike/macos/xcuitest_sandbox.sh`, `docs/spikes/macos-p0.md`).
//!
//! cargo run -p kbf-driver-native --example action_sandbox -- --lease DIR --out DIR
//!
//! The lease directory is made if it is missing, with the lease's home, temporary and
//! cache directories in it, as the driver makes them (`kbf_driver_native::home`). The
//! profile comes from [`Isolation::wrap`] with the network on, so it is
//! [`kbf_driver_native::network::BASE_PROFILE`] followed by the rules for this user's
//! folders ([`UserFolders::rules`], none off macOS): the bytes the driver passes to
//! `sandbox-exec -p`, not a copy of them. Into `--out` go:
//!
//! - `profile.sb`: the profile;
//! - `param`: the `-D` argument that names the lease (`KBF_LEASE=<its real path>`);
//! - `env`: the lease's variables, one `NAME=VALUE` per line.
//!
//! On stdout: `sha256=<hex>` of the profile, so a probe can check that the profile it
//! hands `sandbox-exec` is this one, and `user_folders=<temporary folder>` (or `none`).

use std::error::Error;
use std::path::{Path, PathBuf};

use clap::Parser;
use kbf_driver_native::network::{Isolation, Network, SANDBOX_EXEC};
use kbf_driver_native::user_folders::UserFolders;
use sha2::{Digest as _, Sha256};

#[derive(Parser)]
struct Args {
    /// The lease directory: made if missing.
    #[arg(long)]
    lease: PathBuf,
    /// Where `profile.sb`, `param` and `env` are written: made if missing.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.lease)?;
    std::fs::create_dir_all(&args.out)?;
    let lease = std::fs::canonicalize(&args.lease)?;
    let env = tokio::runtime::Builder::new_current_thread()
        .build()?
        .block_on(kbf_driver_native::home::make(&args.lease))?;
    let folders = UserFolders::detect();
    let rules = folders.as_ref().map(UserFolders::rules).unwrap_or_default();
    let isolation = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
    let (_, wrapped) = isolation.wrap(
        Network::On,
        &lease,
        &rules,
        PathBuf::from("/usr/bin/true"),
        &[],
    );
    let [d, param, p, profile, ..] = wrapped.as_slice() else {
        return Err(format!("Isolation::wrap returned {wrapped:?}").into());
    };
    if d != "-D" || p != "-p" {
        return Err(format!("Isolation::wrap no longer starts -D <param> -p: {wrapped:?}").into());
    }
    let mut lines = String::new();
    for (name, value) in &env {
        let value = value
            .to_str()
            .filter(|v| !v.contains('\n'))
            .ok_or_else(|| format!("{name}={value:?} does not fit on one line"))?;
        lines.push_str(&format!("{name}={value}\n"));
    }
    write(&args.out, "profile.sb", profile)?;
    write(&args.out, "param", &format!("{param}\n"))?;
    write(&args.out, "env", &lines)?;
    println!("sha256={}", hex::encode(Sha256::digest(profile.as_bytes())));
    match &folders {
        Some(folders) => println!("user_folders={}", folders.temp().display()),
        None => println!("user_folders=none"),
    }
    Ok(())
}

fn write(dir: &Path, name: &str, text: &str) -> std::io::Result<()> {
    std::fs::write(dir.join(name), text)
}
