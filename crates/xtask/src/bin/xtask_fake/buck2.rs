//! The buck2 and buildrules personas of `xtask-fake` (see its docs), for the tests of `farm-build`.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

/// The failure asked for, if any: `XTASK_FAKE_DEFECT`, or else the first line of the file `defect`
/// next to this program (a test that runs fakes from its own process cannot set the environment
/// for them).
fn defect() -> String {
    let asked = std::env::var("XTASK_FAKE_DEFECT").unwrap_or_default();
    if !asked.is_empty() {
        return asked;
    }
    let me = std::env::args().next().unwrap_or_default();
    std::fs::read_to_string(Path::new(&me).with_file_name("defect"))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Appends `line` to the file `path`, making it if needed.
fn append(path: &Path, line: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open the log");
    writeln!(file, "{line}").expect("write the log");
}

/// The target of the `buildrules` command line.
const TOOL: &str = "buildrules//cli:buildrules";

/// The fake `buck2`: records the call and answers from the files of the working directory.
pub fn buck2(me: &Path, args: &[String]) -> ExitCode {
    append(Path::new("buck2.calls"), &args.join(" "));
    let verb = args.first().map_or("", String::as_str);
    if verb == "kill" {
        return ExitCode::SUCCESS;
    }
    let key = if args.iter().any(|a| a == TOOL) {
        "tool"
    } else {
        verb
    };
    let read = |suffix: &str, default: &str| {
        std::fs::read_to_string(format!("buck2-{key}.{suffix}"))
            .unwrap_or_else(|_| default.to_owned())
    };
    let tool = format!("{TOOL} {}\n", me.with_file_name("buildrules").display());
    let stdout = match key {
        "audit" => "{}",
        "tool" => &tool,
        _ => "",
    };
    print!("{}", read("stdout", stdout));
    eprint!("{}", read("stderr", ""));
    ExitCode::from(read("status", "0").trim().parse().unwrap_or(2))
}

/// The fake table.
const TABLE: &str = "buildrules.lock.bzl";
/// The fake's one generated file beside the table, there only for a git dependency.
const ARCHIVES: &str = "third_party/buildrules/git_archives.tsv";
/// The files the fake table records.
const INPUTS: [&str; 2] = ["Cargo.toml", "Cargo.lock"];
/// The fix the real command line ends each of its lines with.
const FIX: &str = "run `buildrules sync` and commit the result";

/// The file `path` of the working directory, `None` when it cannot be read.
fn file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The sha256 of the file `path` (of nothing, when it cannot be read), in hex.
fn sha256(path: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(file(path).unwrap_or_default()))
}

/// What a sync of the working directory writes: each generated file, and its text (`None`: the
/// file is removed).
fn synced() -> [(&'static str, Option<String>); 2] {
    let table = INPUTS
        .iter()
        .map(|input| format!("{input} {}\n", sha256(input)))
        .collect();
    let git = file("Cargo.toml").is_some_and(|m| m.contains("git = "));
    [
        (TABLE, Some(table)),
        (ARCHIVES, git.then(|| "github.com/o/r 0123abc\n".to_owned())),
    ]
}

/// The fake `buildrules`: records the call, then syncs or checks as asked.
pub fn buildrules(args: &[String]) -> ExitCode {
    append(Path::new("buildrules.calls"), &args.join(" "));
    let defect = defect();
    if args.first().is_some_and(|verb| verb == "sync") {
        if defect == "sync-fails" {
            eprintln!("buildrules: sync: CargoFailed");
            return ExitCode::from(1);
        }
        if defect != "sync-writes-nothing" {
            for (path, text) in synced() {
                match text {
                    Some(text) => {
                        let path = Path::new(path);
                        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
                        std::fs::write(path, text).expect("write");
                    }
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
        println!("buildrules: wrote {TABLE}");
        return ExitCode::SUCCESS;
    }
    if defect == "check-usage" {
        eprintln!("buildrules: check: unknown argument --nope");
        return ExitCode::from(2);
    }
    let Some(table) = file(TABLE) else {
        eprintln!("buildrules: {TABLE} is missing: {FIX}");
        return ExitCode::from(1);
    };
    let mut stale: Vec<String> = INPUTS
        .iter()
        .filter_map(|input| {
            let now = sha256(input);
            let recorded = table
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{input} ")))
                .unwrap_or("none");
            (recorded != now).then(|| {
                format!("{input} changed since `buildrules sync` (recorded sha256 {recorded}, now {now})")
            })
        })
        .collect();
    if stale.is_empty() {
        stale = synced()
            .into_iter()
            .filter(|(path, text)| file(path) != *text)
            .map(|(path, _)| format!("{path} is not what sync writes"))
            .collect();
    }
    for line in &stale {
        eprintln!("buildrules: STALE: {line}: {FIX}");
    }
    ExitCode::from(u8::from(!stale.is_empty()))
}
