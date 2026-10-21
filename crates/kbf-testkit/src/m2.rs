//! `kbf-m2-act`: the one program the M2 sample project's actions run
//! (`crates/kbf-integration-test/m2/buck2/defs.bzl`). The M2 image, distroless `base-debian12`, holds
//! no shell and no coreutils, so every action runs this program, linked statically,
//! from its input root (CI builds it with `-C target-feature=+crt-static`;
//! `kbf-cell m2` refuses a dynamically linked one). An action that runs it at all
//! proves the farm kept the input file's executable bit.
//!
//! Usage: `kbf-m2-act <verb> <out> <args...>`, writing `<out>`:
//! - `sort OUT IN`: the lines of `IN`, sorted;
//! - `upper OUT IN`: `IN`, ASCII upper-cased;
//! - `join OUT IN...`: the files, one after the other;
//! - `pause OUT SECS SALT`: sleeps `SECS` seconds, then writes `SALT`;
//! - `fill OUT KIB SALT`: writes `KIB` KiB of text, lines of 64 hexadecimal digits, drawn
//!   from a generator seeded by `SALT`: the same salt always gives the same bytes, another
//!   salt gives other bytes. An action that makes new blobs for the farm to store (the beta
//!   harness's uploader, `crates/kbf-integration-test/beta/buck2`);
//! - `hog OUT MIB`: touches `MIB` MiB of distinct, incompressible memory at
//!   [`HOG_MIB_PER_SEC`] and holds all of it until done, then writes `MIB`. Slow on
//!   purpose: a lease over its cap keeps pushing into swap for several of the swap
//!   watch's one-second samples, so the watch sees it (see `kbf_integration_test::m2`).
//!
//! Exits 0 on success, 1 when the verb fails, 2 on a usage error. Tested by running
//! it (`crates/kbf-integration-test/tests/m2_act.rs`).

use std::process::ExitCode;
use std::time::Duration;

use crate::{Error, create, number, read};

/// The hog's touch rate, in MiB per second.
pub const HOG_MIB_PER_SEC: u64 = 256;

/// Runs `kbf-m2-act` on the process's arguments.
#[must_use]
pub fn main() -> ExitCode {
    let (_, args) = crate::arguments();
    ExitCode::from(crate::status(
        "kbf-m2-act",
        args.and_then(|args| run(&args)),
    ))
}

fn run(args: &[String]) -> Result<u8, Error> {
    let [verb, out, rest @ ..] = args else {
        return Err(Error::Usage(
            "usage: kbf-m2-act <verb> <out> <args...>".to_owned(),
        ));
    };
    let bytes = match (verb.as_str(), rest) {
        ("sort", [input]) => sort(&read(input)?),
        ("upper", [input]) => read(input)?.to_ascii_uppercase(),
        ("join", inputs) if !inputs.is_empty() => {
            let mut all = Vec::new();
            for input in inputs {
                all.extend(read(input)?);
            }
            all
        }
        ("pause", [secs, salt]) => {
            std::thread::sleep(Duration::from_secs(number(secs)?));
            format!("{salt}\n").into_bytes()
        }
        ("fill", [kib, salt]) => fill(number(kib)?, salt)?,
        ("hog", [mib]) => {
            let mib = number(mib)?;
            drop(crate::touch(mib, crate::pace(HOG_MIB_PER_SEC)));
            format!("{mib}\n").into_bytes()
        }
        _ => {
            return Err(Error::Usage(format!(
                "bad arguments for {verb:?}: {rest:?}"
            )));
        }
    };
    create(out, &bytes)?;
    Ok(0)
}

/// The most KiB `fill` writes: an action that asks for more is a mistake in the project.
const FILL_MAX_KIB: u64 = 1 << 20;

/// `kib` KiB of lines of 64 hexadecimal digits from a xorshift generator seeded by an FNV-1a
/// hash of `salt` (so the bytes are a function of the salt alone).
fn fill(kib: u64, salt: &str) -> Result<Vec<u8>, Error> {
    if kib > FILL_MAX_KIB {
        return Err(Error::Usage(format!(
            "fill: {kib} KiB is over {FILL_MAX_KIB}"
        )));
    }
    let hash = salt.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    // A zero state would stay zero.
    let mut state = hash | 1;
    // At most 1 GiB: it fits a `usize` anywhere.
    let want = (kib * 1024) as usize;
    let mut out = String::with_capacity(want + 65);
    while out.len() < want {
        for _ in 0..4 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.push_str(&format!("{state:016x}"));
        }
        out.push('\n');
    }
    out.truncate(want);
    Ok(out.into_bytes())
}

/// The lines of `text` in byte order, each ending in a newline.
fn sort(text: &[u8]) -> Vec<u8> {
    let mut lines: Vec<&[u8]> = text
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort_unstable();
    let mut out = Vec::with_capacity(text.len() + 1);
    for line in lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}
