//! `kbf-m2-act`: the one program the M2 sample project's actions run
//! (`m2/buck2/defs.bzl`). The M2 image, distroless `base-debian12`, holds no shell and
//! no coreutils, so every action runs this binary, linked statically, from its input
//! root (CI builds it with `-C target-feature=+crt-static`; `m2/run.sh` refuses a
//! dynamically linked one). An action that runs it at all proves the farm kept the
//! input file's executable bit.
//!
//! Usage: `kbf-m2-act <verb> <out> <args...>`, writing `<out>`:
//! - `sort OUT IN`: the lines of `IN`, sorted;
//! - `upper OUT IN`: `IN`, ASCII upper-cased;
//! - `join OUT IN...`: the files, one after the other;
//! - `pause OUT SECS SALT`: sleeps `SECS` seconds, then writes `SALT`;
//! - `hog OUT MIB`: touches `MIB` MiB of distinct, incompressible memory at
//!   [`HOG_MIB_PER_SEC`] and holds all of it until done, then writes `MIB`. Slow on
//!   purpose: a lease over its cap keeps pushing into swap for several of the swap
//!   watch's one-second samples, so the watch sees it (see `m2/run.sh`).
//!
//! Exits 0 on success, 1 when the verb fails, 2 on a usage error. Tested by running it
//! (`tests/m2_act.rs`), so coverage counts one build of it.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

/// The hog's touch rate, in MiB per second.
const HOG_MIB_PER_SEC: u64 = 256;
/// The hog's step, in MiB: it touches one chunk, then pauses.
const HOG_CHUNK_MIB: u64 = 16;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(why)) => {
            eprintln!("kbf-m2-act: {why}");
            ExitCode::from(2)
        }
        Err(Error::Failed(why)) => {
            eprintln!("kbf-m2-act: {why}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Error {
    Usage(String),
    Failed(String),
}

fn run(args: &[String]) -> Result<(), Error> {
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
        ("hog", [mib]) => {
            let mib = number(mib)?;
            hog(mib);
            format!("{mib}\n").into_bytes()
        }
        _ => {
            return Err(Error::Usage(format!(
                "bad arguments for {verb:?}: {rest:?}"
            )));
        }
    };
    let mut file =
        std::fs::File::create(out).map_err(|e| Error::Failed(format!("create {out}: {e}")))?;
    file.write_all(&bytes)
        .map_err(|e| Error::Failed(format!("write {out}: {e}")))
}

fn read(path: &str) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::Failed(format!("read {path}: {e}")))
}

fn number(text: &str) -> Result<u64, Error> {
    text.parse()
        .map_err(|_| Error::Usage(format!("{text:?} is not a whole number")))
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

/// Touches `mib` MiB, [`HOG_CHUNK_MIB`] at a time at [`HOG_MIB_PER_SEC`], every word a
/// different value from a xorshift sequence, so no page is a zero page, a duplicate or
/// compressible; all of it stays allocated until the end.
fn hog(mib: u64) {
    let words_per_chunk = usize::try_from((HOG_CHUNK_MIB << 20) / 8).unwrap_or(usize::MAX);
    let pause = Duration::from_millis(1000 * HOG_CHUNK_MIB / HOG_MIB_PER_SEC);
    let mut held: Vec<Vec<u64>> = Vec::new();
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut touched = 0;
    while touched < mib {
        let mut chunk = vec![0u64; words_per_chunk];
        for word in &mut chunk {
            state = xorshift(state);
            *word = state;
        }
        held.push(chunk);
        touched += HOG_CHUNK_MIB;
        std::thread::sleep(pause);
    }
    // Read it back, so no write above can be optimised away.
    let sum = held
        .iter()
        .flatten()
        .fold(0u64, |acc, w| acc.wrapping_add(*w));
    std::hint::black_box(sum);
}

const fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}
