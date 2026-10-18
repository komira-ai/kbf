//! `cargo xtask gamma`: the steps of `.github/workflows/gamma.yml`, which deploys a push
//! that built and attested to the operator's own farm (docs/design/gamma-deploy.md).
//!
//! - `gamma prepare --commit <sha> --host <h> --user <u> --repo <owner/repo> --dist <dir>
//!   --work <dir> --out <tar> [--gh <program>]` ([`prepare`]): masks the host and user in
//!   the log, then packs into `<tar>` the three tarballs the farm runs for the commit
//!   ([`tarballs`]), each with its build-attestation bundle from `gh attestation
//!   download`, named as the release store names them. A missing tarball, or a download
//!   that leaves other than one bundle, is an error. The bundles are bytes to carry, not
//!   trust: the host verifies them offline.
//! - `gamma deploy --commit <sha> --host <h> --user <u> --bundle <tar> --summary <file>
//!   [--tailscale <program>]` ([`deploy`]): `tailscale ssh <user>@<host> sudo
//!   kbf-installer deploy --apply --json --commit <sha> --bundle -` with the tar as its
//!   standard input and its standard output (the one-line summary) in `<file>`. Fails when
//!   the deploy's exit status is not 0 (the status is in the message and the summary); no
//!   message names the host.
//! - `gamma summary --file <summary.json> --output <file>` ([`summary`]): a table of the
//!   nodes by platform and state, and the steps, appended to `<file>` (on Actions,
//!   `GITHUB_STEP_SUMMARY`). It prints no node id and no host. A missing or unreadable
//!   summary is said so; it is not an error, so it never hides the deploy's own failure.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::Error;
use crate::build_commit::is_commit;

/// The most an attestation bundle may be (the release store's cap).
const MAX_ATTESTATION: usize = 256 * 1024;

/// The tarballs of a commit the farm runs, `<name>-<commit>-<platform>.tar.gz`: `kbf` on
/// Linux, `kbf-daemon` on macOS (docs/design/pull-updates.md, section 5).
pub fn tarballs(commit: &str) -> Vec<String> {
    [
        ("kbf", "linux-x86_64"),
        ("kbf", "linux-arm64"),
        ("kbf-daemon", "darwin-arm64"),
    ]
    .iter()
    .map(|(name, platform)| format!("{name}-{commit}-{platform}.tar.gz"))
    .collect()
}

fn full_commit(commit: &str) -> Result<(), Error> {
    if commit.len() == 40 && is_commit(commit) {
        return Ok(());
    }
    Err(Error::Usage(format!(
        "--commit {commit:?} is not a full commit SHA (40 lowercase hex digits)"
    )))
}

fn nonempty(flag: &str, value: &str) -> Result<(), Error> {
    if value.trim().is_empty() {
        return Err(Error::Usage(format!("{flag} is empty")));
    }
    Ok(())
}

/// Fetch the attestation bundle of `file` into the empty directory `into`.
pub type Download<'a> = dyn Fn(&Path, &Path) -> Result<(), Error> + 'a;

/// `gh attestation download <file> -R <repo>` in `into`.
pub fn gh_download<'a>(
    gh: &'a Path,
    repo: &'a str,
) -> impl Fn(&Path, &Path) -> Result<(), Error> + 'a {
    move |file, into| {
        let out = Command::new(gh)
            .args(["attestation", "download"])
            .arg(file)
            .args(["-R", repo])
            .current_dir(into)
            .output()
            .map_err(Error::spawn(gh.display().to_string()))?;
        if out.status.success() {
            return Ok(());
        }
        Err(Error::Failed(format!(
            "gh attestation download {}: {}",
            file.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            String::from_utf8_lossy(&out.stderr).trim_end()
        )))
    }
}

/// The `gamma prepare` verb: the lines to print (the log masks first).
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    download: &Download<'_>,
    commit: &str,
    host: &str,
    user: &str,
    dist: &Path,
    work: &Path,
    out: &Path,
) -> Result<Vec<String>, Error> {
    full_commit(commit)?;
    nonempty("--host", host)?;
    nonempty("--user", user)?;
    // Printed first so that nothing later can show them: a workflow command, not a log line.
    let mut lines = vec![format!("::add-mask::{host}"), format!("::add-mask::{user}")];
    let mut builder = tar::Builder::new(Vec::new());
    for (n, name) in tarballs(commit).iter().enumerate() {
        let path = dist.join(name);
        let bytes = fs::read(&path).map_err(Error::spawn(format!("read {name}")))?;
        let scratch = work.join(format!("gamma-attestation-{n}"));
        fs::create_dir_all(&scratch)
            .map_err(Error::spawn(format!("create {}", scratch.display())))?;
        let held = fs::read_dir(&scratch)
            .map_err(Error::spawn(format!("read {}", scratch.display())))?
            .count();
        if held != 0 {
            return Err(Error::Failed(format!("{} is not empty", scratch.display())));
        }
        download(&path, &scratch)?;
        let found: Vec<PathBuf> = fs::read_dir(&scratch)
            .map_err(Error::spawn(format!("read {}", scratch.display())))?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()
            .map_err(Error::spawn(format!("read {}", scratch.display())))?;
        let [bundle] = found.as_slice() else {
            return Err(Error::Failed(format!(
                "expected one attestation bundle for {name}, found {}",
                found.len()
            )));
        };
        let attestation =
            fs::read(bundle).map_err(Error::spawn(format!("read the bundle of {name}")))?;
        if attestation.is_empty() || attestation.len() > MAX_ATTESTATION {
            return Err(Error::Failed(format!(
                "the attestation bundle of {name} is empty or over {MAX_ATTESTATION} bytes"
            )));
        }
        add(&mut builder, name, &bytes)?;
        add(&mut builder, &format!("{name}.attestation"), &attestation)?;
        lines.push(format!(
            "packed {name} ({} bytes) with its attestation",
            bytes.len()
        ));
    }
    let tar = builder
        .into_inner()
        .map_err(Error::spawn("finish the tar".to_owned()))?;
    fs::write(out, tar).map_err(Error::spawn(format!("write {}", out.display())))?;
    Ok(lines)
}

fn add(builder: &mut tar::Builder<Vec<u8>>, name: &str, bytes: &[u8]) -> Result<(), Error> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, name, bytes)
        .map_err(Error::spawn(format!("pack {name}")))
}

/// The arguments of `tailscale` for the deploy.
pub fn ssh_args(commit: &str, host: &str, user: &str) -> Vec<String> {
    [
        "ssh",
        &format!("{user}@{host}"),
        "sudo",
        "kbf-installer",
        "deploy",
        "--apply",
        "--json",
        "--commit",
        commit,
        "--bundle",
        "-",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Run `program args` with `bundle` as its standard input and its standard output in
/// `summary`. Returns its exit status (`None` when a signal ended it).
pub fn pipe(
    program: &Path,
    args: &[String],
    bundle: &Path,
    summary: &Path,
) -> Result<Option<i32>, Error> {
    let input =
        fs::File::open(bundle).map_err(Error::spawn(format!("open {}", bundle.display())))?;
    let output =
        fs::File::create(summary).map_err(Error::spawn(format!("create {}", summary.display())))?;
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output))
        .status()
        .map_err(Error::spawn("tailscale".to_owned()))?;
    Ok(status.code())
}

/// The `gamma deploy` verb.
pub fn deploy(
    program: &Path,
    commit: &str,
    host: &str,
    user: &str,
    bundle: &Path,
    summary: &Path,
) -> Result<Vec<String>, Error> {
    full_commit(commit)?;
    nonempty("--host", host)?;
    nonempty("--user", user)?;
    let code = pipe(program, &ssh_args(commit, host, user), bundle, summary)?;
    match code {
        Some(0) => Ok(vec!["kbf-installer deploy exited 0".to_owned()]),
        Some(n) => Err(Error::Failed(format!(
            "kbf-installer deploy exited {n} (see the summary)"
        ))),
        None => Err(Error::Failed(
            "the deploy connection ended by a signal".to_owned(),
        )),
    }
}

/// The markdown of a summary, or why it cannot be shown. It names no node and no host.
pub fn table(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return "### gamma deploy\n\nno summary was printed\n".to_owned();
    };
    let text = |p: &str| {
        v.pointer(p)
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned()
    };
    let mut out = format!(
        "### gamma deploy of `{}`\n\nresult: {} (exit {}) {}\n\n",
        text("/commit"),
        text("/result"),
        v.pointer("/exit")
            .and_then(Value::as_i64)
            .map_or_else(|| "?".to_owned(), |n| n.to_string()),
        text("/reason").replace('?', ""),
    );
    out.push_str("| platform | state | seconds |\n|---|---|---|\n");
    for n in v
        .pointer("/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let cell = |k: &str| {
            n.get(k).map_or_else(
                || "?".to_owned(),
                |c| c.as_str().map_or_else(|| c.to_string(), str::to_owned),
            )
        };
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            cell("platform"),
            cell("state"),
            cell("secs")
        ));
    }
    out.push_str("\nsteps: ");
    let steps: Vec<String> = v
        .pointer("/steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| {
            let ok = s.get("ok").and_then(Value::as_bool) == Some(true);
            format!(
                "{}={}",
                s.get("name").and_then(Value::as_str).unwrap_or("?"),
                if ok { "ok" } else { "FAILED" }
            )
        })
        .collect();
    out.push_str(&steps.join(" "));
    out.push('\n');
    out
}

/// The `gamma summary` verb.
pub fn summary(file: &Path, output: &Path) -> Result<Vec<String>, Error> {
    let json = fs::read_to_string(file).unwrap_or_default();
    let md = table(&json);
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output)
        .map_err(Error::spawn(format!("open {}", output.display())))?;
    out.write_all(md.as_bytes())
        .map_err(Error::spawn(format!("write {}", output.display())))?;
    Ok(vec![format!("wrote {} bytes", md.len())])
}

#[cfg(test)]
mod tests;
