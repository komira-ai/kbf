//! `kbf-podman-stub`: a `podman` that knows no container, for `tests/binary.rs`, which
//! links it as `podman` into a directory it puts first in `PATH`. It prints nothing,
//! succeeds, and logs its arguments, one per line, to `podman.log` beside the link it
//! is run through. It knows an image only while `image-id` beside the link exists:
//! `image inspect` prints that file, whatever image it is asked about, and fails as
//! Podman does for an image not in the store (exit 125) when the file is absent.
//! `info` names `store/` beside the link as the directory of the image store's
//! per-image directories, and the graph driver, asked with its options, as the
//! kernel's `overlay`.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().expect("an argv0");
    let args: Vec<String> = args.collect();
    // The link this was run through: argv0 itself, or as `PATH` finds a bare name.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let link = std::iter::once(PathBuf::from(&argv0))
        .chain(std::env::split_paths(&path).map(|dir| dir.join(&argv0)))
        .find(|p| p.is_absolute() && p.is_file())
        .expect("the link this was run through");
    let here = link.parent().expect("the link's directory");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(here.join("podman.log"))
        .expect("open podman.log");
    for arg in &args {
        writeln!(log, "{arg}").expect("write podman.log");
    }
    let verb = args.iter().find(|a| !a.starts_with('-'));
    match verb.map(String::as_str) {
        Some("image") => match std::fs::read(here.join("image-id")) {
            Ok(id) => std::io::stdout()
                .write_all(&id)
                .expect("write the image id"),
            Err(_) => {
                eprintln!("Error: {}: image not known", here.display());
                return ExitCode::from(125);
            }
        },
        // The input cache's question: the kernel's overlay, with no mount program.
        Some("info") if args.iter().any(|a| a.contains("GraphOptions")) => println!("overlay {{}}"),
        Some("info") => println!("{}", here.join("store").display()),
        _ => {}
    }
    ExitCode::SUCCESS
}
