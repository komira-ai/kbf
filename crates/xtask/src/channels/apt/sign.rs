//! `channels apt sign --dir <dir> --work <dir> --fingerprint <fpr>[,<fpr>...]
//! (--key-file <file> | --key-env <name>) (--passphrase-file <file> | --passphrase-env <name>)
//! [--gpg <program>] [--output <file>]`: `InRelease` (clear signed) and `Release.gpg` (detached)
//! of `<dir>/Release`, verified.
//!
//! The index is signed with the signing subkey of the archive key: the key spec is the subkey's
//! fingerprint followed by `!`, which makes gpg use exactly that key and no other. Several
//! fingerprints (a key rotation) give one signature per key, in both files. The passphrase is
//! never an argument (arguments are readable by every process of the machine): it is a file, with
//! the loopback pinentry. The key and the passphrase come as a file the job wrote
//! (`--key-file`, `--passphrase-file`) or as the name of an environment variable that holds the
//! secret material (`--key-env`, `--passphrase-env`), which the verb writes to a file of mode 0600
//! below `--work` itself; a variable that is not set or is empty is refused. The files and the
//! keyring in a private directory (0700) below `--work` are removed when the verb ends, whatever
//! happened. A fingerprint that is not among the secret keys imported fails before
//! anything is signed. `signed=true` is appended to `--output` when both files verify.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::channels::Error;
use crate::channels::aws::Answer;

/// The `gpg` program, behind a trait so a test answers in place of it.
pub trait Gpg {
    /// `gpg <args>`. A refusal is an [`Answer`] that is not `ok`.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, args: &[String]) -> Result<Answer, Error>;
}

/// The `gpg` program at a path.
pub struct RealGpg(pub PathBuf);

impl Gpg for RealGpg {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        let out = Command::new(&self.0)
            .args(args)
            .output()
            .map_err(Error::spawn(self.0.display().to_string()))?;
        Ok(Answer {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Where a piece of secret material comes from.
#[derive(Clone, Copy)]
pub enum Source<'a> {
    /// A file the job wrote; removed at the end.
    File(&'a Path),
    /// The name of an environment variable that holds it; written to a file of mode 0600 below
    /// `--work`, which is removed at the end.
    Env(&'a str),
}

impl<'a> Source<'a> {
    /// The source named by exactly one of the two flags `--<what>-file` and `--<what>-env`.
    ///
    /// # Errors
    /// None or both of them are given.
    pub fn choose(
        what: &str,
        file: Option<&'a Path>,
        env: Option<&'a str>,
    ) -> Result<Source<'a>, Error> {
        match (file, env) {
            (Some(file), None) => Ok(Source::File(file)),
            (None, Some(name)) => Ok(Source::Env(name)),
            _ => Err(Error::Usage(format!(
                "give exactly one of --{what}-file and --{what}-env"
            ))),
        }
    }

    /// The file that holds the material: the given file, or `<work>/<name>`.
    fn path(&self, work: &Path, name: &str) -> PathBuf {
        match self {
            Source::File(file) => file.to_path_buf(),
            Source::Env(_) => work.join(name),
        }
    }

    /// Writes the material of an environment variable to `path`, readable by this user alone
    /// from the first byte; a file source is left as it is.
    fn write(
        &self,
        path: &Path,
        work: &Path,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), Error> {
        let Source::Env(name) = self else {
            return Ok(());
        };
        let value = env(name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::Usage(format!("{name} is not set, or is empty")))?;
        let made = std::fs::create_dir_all(work)
            .and_then(|()| match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            })
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
            })
            .and_then(|mut file| file.write_all(value.as_bytes()));
        made.map_err(Error::spawn(format!("write {}", path.display())))
    }
}

/// The value of the environment variable `name` of this process, when it is set to text. The
/// secret material of a signing job comes this way and no other configuration does.
#[must_use]
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// What `apt sign` is asked.
pub struct Request<'a> {
    /// `dists/<suite>` of the new index.
    pub dir: &'a Path,
    /// Where the private keyring is made; it is removed at the end.
    pub work: &'a Path,
    /// The signing subkey's fingerprint, or several separated by commas.
    pub fingerprints: &'a str,
    /// The armored private key; its file is removed at the end.
    pub key: Source<'a>,
    /// The key's passphrase; its file is removed at the end.
    pub passphrase: Source<'a>,
}

/// The files of the key and the passphrase: the given files, or the ones made below `--work`.
fn secret_files(r: &Request) -> (PathBuf, PathBuf) {
    (
        r.key.path(r.work, "apt-key.asc"),
        r.passphrase.path(r.work, "apt-passphrase"),
    )
}

/// A fingerprint as hex digits (the spaces gpg prints between groups are tolerated), at least 16
/// of them: upper case, without spaces.
pub(super) fn clean_fingerprint(raw: &str) -> Option<String> {
    let clean: String = raw.chars().filter(|c| *c != ' ').collect();
    (clean.len() >= 16 && clean.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| clean.to_ascii_uppercase())
}

/// The fingerprints of the signing subkeys: one, or several separated by commas. A list that is
/// empty, has an empty element or names a key twice is refused.
pub(crate) fn split_fingerprints(raw: &str) -> Result<Vec<String>, Error> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        match clean_fingerprint(part) {
            Some(fpr) if !out.contains(&fpr) => out.push(fpr),
            _ => {
                return Err(Error::Usage(
                    "--fingerprint is not a fingerprint, or a comma-separated list of different ones"
                        .to_owned(),
                ));
            }
        }
    }
    Ok(out)
}

pub(crate) fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// The gpg half: imports the key, checks every fingerprint is a secret key, signs both files and
/// verifies them.
fn gpg_steps(
    gpg: &dyn Gpg,
    home: &Path,
    fingerprints: &[String],
    r: &Request,
    (key_file, passphrase_file): &(PathBuf, PathBuf),
) -> Result<(), Error> {
    let text = |p: &Path| p.display().to_string();
    let home_arg = text(home);
    let base = |extra: &[&str]| {
        let mut v = args(&["--homedir", &home_arg, "--batch", "--no-tty", "--yes"]);
        v.extend(args(extra));
        v
    };
    let (release, in_release, detached) = (
        text(&r.dir.join("Release")),
        text(&r.dir.join("InRelease")),
        text(&r.dir.join("Release.gpg")),
    );
    let run = |what: &str, argv: Vec<String>| -> Result<(), Error> {
        let answer = gpg.run(&argv)?;
        if answer.ok {
            Ok(())
        } else {
            Err(Error::Failed(format!("{what}: {}", answer.stderr.trim())))
        }
    };
    run(
        "gpg could not import the key",
        base(&["--import", &text(key_file)]),
    )?;
    for fpr in fingerprints {
        run(
            &format!("the signing key is not among the secret keys that were imported ({fpr})"),
            base(&["--list-secret-keys", "--with-colons", fpr]),
        )?;
    }
    let signing = |mode: &str, input: &str, output: &str| {
        let mut v = base(&[
            "--pinentry-mode",
            "loopback",
            "--passphrase-file",
            &text(passphrase_file),
            "--digest-algo",
            "SHA512",
        ]);
        for fpr in fingerprints {
            v.extend(args(&["--local-user", &format!("{fpr}!")]));
        }
        v.extend(args(&["--armor", mode, "--output", output, input]));
        v
    };
    run(
        "gpg could not sign InRelease",
        signing("--clearsign", &release, &in_release),
    )?;
    run(
        "gpg could not sign Release.gpg",
        signing("--detach-sign", &release, &detached),
    )?;
    run(
        "InRelease does not verify",
        base(&["--verify", &in_release]),
    )?;
    run(
        "Release.gpg does not verify",
        base(&["--verify", &detached, &release]),
    )
}

/// An empty directory `home` (mode 0700) below `work`, which is made when missing.
pub(crate) fn private_dir(work: &Path, home: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(home) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::create_dir_all(work)?;
    std::fs::DirBuilder::new().mode(0o700).create(home)
}

/// The `apt sign` verb: the lines to print. `env` answers the environment variables the key and
/// the passphrase may be named by.
///
/// # Errors
/// A variable that is not set, a fingerprint that is not one, a `Release` that is not there, a
/// private directory that cannot be made, or a gpg step that fails.
pub fn sign(
    gpg: &dyn Gpg,
    r: &Request,
    env: &dyn Fn(&str) -> Option<String>,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let files = secret_files(r);
    let done = r
        .key
        .write(&files.0, r.work, env)
        .and_then(|()| r.passphrase.write(&files.1, r.work, env))
        .and_then(|()| sign_files(gpg, r, &files));
    // The key never outlives the verb, whatever the verb came to (a flag out of its form
    // included). A runner is discarded after its job all the same, so a file that is gone already
    // is not a failure.
    let _ = std::fs::remove_dir_all(r.work.join("gnupg"));
    let _ = std::fs::remove_file(&files.0);
    let _ = std::fs::remove_file(&files.1);
    let fingerprints = done?;
    if let Some(output) = output {
        super::super::append_output(output, &["signed=true".to_owned()])?;
    }
    Ok(vec![format!(
        "signed Release with {} subkey(s), {}: InRelease and Release.gpg verify",
        fingerprints.len(),
        fingerprints.join(", ")
    )])
}

/// The signing itself: the fingerprints that signed.
fn sign_files(
    gpg: &dyn Gpg,
    r: &Request,
    files: &(PathBuf, PathBuf),
) -> Result<Vec<String>, Error> {
    let fingerprints = split_fingerprints(r.fingerprints)?;
    let release = r.dir.join("Release");
    if !release.is_file() {
        return Err(Error::Failed(format!(
            "{} does not exist: nothing to sign",
            release.display()
        )));
    }
    let home = r.work.join("gnupg");
    private_dir(r.work, &home).map_err(Error::spawn(format!("create {}", home.display())))?;
    gpg_steps(gpg, &home, &fingerprints, r, files)?;
    Ok(fingerprints)
}

#[cfg(test)]
mod tests;
