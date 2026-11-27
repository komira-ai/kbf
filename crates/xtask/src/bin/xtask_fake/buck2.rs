//! The buck2, buildrules and assert_remote personas of `xtask-fake` (see its docs), for the tests of
//! `farm-build` and `prove-farm`.

use std::io::{Read, Write};
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

/// The lines the stand-in `assert_remote` prints for a first run its watchdog ended (defect
/// `watchdog-cut`), the words of the real tool's.
pub const CUT: &str = "\
buck2-farm: watchdog: the connection to the farm from local port 41022 has heard nothing from the farm for 5 min 1 s, past the bound of 5 min 0 s: the farm has closed it and buck2 would wait on it for ever; ending buck2 (SIGINT)
buck2-farm: stopping buck2's daemon, which holds the dead connection (`buck2 kill`)
buck2-farm: the first run hung on a dead connection to the farm and was ended; waiting for the farm's port to answer, up to 10 min 0 s
";

/// The line the stand-in `assert_remote` prints when the build's every run hung (defect
/// `watchdog-gives-up`), before it exits 1.
pub const GAVE_UP: &str =
    "buck2-farm: the third run hung on a dead connection to the farm too; not running it again\n";

/// The fake `assert_remote run --log <file> --summary <file> --farm-config <file> -- <buck2>
/// <args>`: records the call, then runs the command once with its standard output and error on
/// one pipe, copied to its own standard output and to `--log`, and exits with the command's
/// status. A call of another shape exits 2. Defects: `watchdog-cut` (every call first prints
/// [`CUT`] and runs `<buck2> kill`, as after a run the watchdog ended, then runs the command) and
/// `watchdog-gives-up` (a call that is not the build of the `buildrules` command line runs
/// `<buck2> kill`, prints [`GAVE_UP`] and exits 1 without running the command).
pub fn assert_remote(args: &[String]) -> ExitCode {
    append(Path::new("assert_remote.calls"), &args.join(" "));
    let Some(dash) = args.iter().position(|a| a == "--") else {
        eprintln!("assert_remote: no `--` before the command");
        return ExitCode::from(2);
    };
    let flag = |name: &str| {
        args[..dash]
            .windows(2)
            .find(|w| w[0] == name)
            .map(|w| w[1].clone())
    };
    let (Some("run"), Some(log), Some(_), Some(_), Some(program)) = (
        args.first().map(String::as_str),
        flag("--log"),
        flag("--summary"),
        flag("--farm-config"),
        args.get(dash + 1),
    ) else {
        eprintln!(
            "assert_remote: usage: run --log <file> --summary <file> --farm-config <file> -- <command>"
        );
        return ExitCode::from(2);
    };
    let command = &args[dash + 2..];
    let defect = defect();
    let kill = || {
        let _ = std::process::Command::new(program).arg("kill").status();
    };
    if defect == "watchdog-gives-up" && !command.iter().any(|a| a == TOOL) {
        kill();
        print!("{GAVE_UP}");
        return ExitCode::FAILURE;
    }
    if defect == "watchdog-cut" {
        print!("{CUT}");
        kill();
    }
    let (mut reader, writer) = std::io::pipe().expect("pipe");
    let mut child = {
        let mut run = std::process::Command::new(program);
        run.args(command)
            .stdout(writer.try_clone().expect("pipe"))
            .stderr(writer);
        run.spawn().expect("run the command")
    };
    let mut printed = Vec::new();
    reader.read_to_end(&mut printed).expect("read the command");
    let status = child.wait().expect("wait for the command");
    std::io::stdout().write_all(&printed).expect("print");
    std::fs::write(&log, &printed).expect("write the log");
    ExitCode::from(u8::try_from(status.code().unwrap_or(2)).unwrap_or(2))
}
