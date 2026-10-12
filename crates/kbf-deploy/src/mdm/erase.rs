//! `kbf-deploy mdm erase-request`: an operator's erase request for one Mac, signed with
//! the operator's security key, for the MDM gate (`kbf-mdm-gate`).
//!
//! The request is the gate's own `kbf-erase-v1` text, written and parsed by
//! [`kbf_mdm::request`], so the bytes signed here are bytes the gate's parser accepts.
//! It is signed by `ssh-keygen -Y sign -n kbf-mdm-erase -f KEY`, run as argv by
//! absolute path with an empty environment, the request on its standard input and the
//! signature on its standard output; the security key asks for its touch (and its PIN
//! on the terminal, when the key needs one). The signature is written to `OUT.sig`,
//! then the request to `OUT`.
//!
//! After an apply both files are read back and checked in process as the gate checks
//! them ([`kbf_mdm::signers::AllowedSigners::verify`], with the key's own public half
//! as the only allowed signer): the text parses as the request asked for, and the
//! signature is over exactly those bytes, in the gate's namespace, by a security key
//! (`sk-`), with the touch flag set. A pair that fails is reported and exits 1.
//!
//! The verb runs as the operator, never as root, and writes only `OUT` and `OUT.sig`,
//! which must not exist yet: a signed request is never overwritten.

use std::io::{self, Write};

use kbf_host::exec::{Entry, Exec};
use kbf_host::path::HostPath;
use kbf_host::plan::{Mode, Planner, RunAs, execute};
use kbf_host::step::{Cmd, Filter, Perms};
use kbf_mdm::clock::format_rfc3339;
use kbf_mdm::request::{self, EraseRequest, Purpose, RequestError};
use kbf_mdm::signers::{AllowedSigners, FLAG_USER_VERIFIED, NAMESPACE, Signer, VerifyError};
use ssh_key::{Algorithm, PublicKey, SshSig};

use super::MdmError;

/// The longest a request may stay acceptable, in minutes: the gate refuses a
/// `not-after` more than an hour ahead.
pub const MAX_VALID_FOR: u32 = 60;

/// The `ssh-keygen` of an operator's system.
pub const SSH_KEYGEN: &str = "/usr/bin/ssh-keygen";

/// The principal of the one allowed-signers line the check after signing builds: any
/// name serves, as that line holds only the operator's own key.
const SELF: &str = "operator";

/// What `mdm erase-request` is asked to sign.
#[derive(Clone, Debug)]
pub struct EraseArgs {
    /// The Mac's serial number.
    pub serial: String,
    /// `Some(lease)`: hold the erase for that privileged lease; `None`: erase now.
    pub lease: Option<String>,
    /// Why, in plain words; the gate's alerts quote it.
    pub reason: String,
    /// The security key's key file (what `ssh-keygen -f` takes); its public half is
    /// beside it as `<key>.pub`.
    pub key: HostPath,
    /// Where the request goes; the signature goes to `<out>.sig`.
    pub out: HostPath,
    /// For how many minutes from now the gate may accept the request.
    pub valid_for: u32,
    /// The `ssh-keygen` to sign with.
    pub ssh_keygen: HostPath,
}

/// Why `mdm erase-request` refused, or why its signed pair does not check.
#[derive(Debug, thiserror::Error)]
pub enum EraseError {
    #[error("--valid-for {0}: a request is valid for 1 to {MAX_VALID_FOR} minutes")]
    ValidFor(u32),
    #[error("the gate would refuse this request: {0}")]
    Request(CheckError),
    #[error(
        "{0} does not exist: --key names the security key's key file, with its public half beside it as <key>.pub"
    )]
    NoPublicKey(HostPath),
    #[error("{path} is not an OpenSSH public key: {reason}")]
    PublicKey { path: HostPath, reason: String },
    #[error(
        "{path} is a {algorithm} key; the gate accepts only security keys (sk-ssh-ed25519@openssh.com, sk-ecdsa-sha2-nistp256@openssh.com)"
    )]
    SoftwareKey { path: HostPath, algorithm: String },
    #[error("{0} already exists: a signed request is never overwritten; choose another --out")]
    Exists(HostPath),
    #[error("{out} and {sig} would be refused by the gate, do not send them: {why}")]
    Check {
        out: HostPath,
        sig: HostPath,
        why: CheckError,
    },
}

/// What is wrong with a request's text, or with a signed pair read back after signing.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    #[error("{0} is not a file")]
    Missing(HostPath),
    #[error("{0} is not UTF-8 text")]
    NotText(&'static str),
    #[error("{0}")]
    Parse(#[from] RequestError),
    #[error("the request on disk is not the one asked for")]
    Differs,
    #[error(transparent)]
    Verify(#[from] VerifyError),
}

/// `bytes` are a request the gate parses, and that request is `request`.
pub fn check_text(bytes: &[u8], request: &EraseRequest) -> Result<(), CheckError> {
    let text = std::str::from_utf8(bytes).map_err(|_| CheckError::NotText("the request"))?;
    if request::parse(text)? == *request {
        Ok(())
    } else {
        Err(CheckError::Differs)
    }
}

/// What is at `path`, through `exec`.
fn inspect<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Entry>, MdmError> {
    exec.inspect(path).map_err(|source| MdmError::Io {
        path: path.as_str().into(),
        source,
    })
}

/// The security key's public half, from `<key>.pub`, refused unless it is an `sk-` key
/// the gate accepts.
fn public_key<E: Exec>(exec: &E, key: &HostPath) -> Result<PublicKey, MdmError> {
    let path = HostPath::new(&format!("{key}.pub"))?;
    let bytes = match inspect(exec, &path)? {
        Some(Entry::File { bytes, .. }) => bytes,
        Some(_) => {
            let reason = "not a file".to_owned();
            return Err(EraseError::PublicKey { path, reason }.into());
        }
        None => return Err(EraseError::NoPublicKey(path).into()),
    };
    let public = match PublicKey::from_openssh(String::from_utf8_lossy(&bytes).trim()) {
        Ok(public) => public,
        Err(e) => {
            let reason = e.to_string();
            return Err(EraseError::PublicKey { path, reason }.into());
        }
    };
    let algorithm = public.algorithm();
    if !matches!(
        algorithm,
        Algorithm::SkEd25519 | Algorithm::SkEcdsaSha2NistP256
    ) {
        let algorithm = algorithm.as_str().to_owned();
        return Err(EraseError::SoftwareKey { path, algorithm }.into());
    }
    Ok(public)
}

/// `ssh-keygen -Y sign` of `text`, given on standard input, by `key` in the gate's
/// namespace; the signature comes out on standard output.
pub fn sign_cmd(ssh_keygen: &HostPath, key: &HostPath, text: &str) -> Cmd {
    Cmd::new(ssh_keygen.clone())
        .arg("-Y")
        .arg("sign")
        .arg("-n")
        .arg(NAMESPACE)
        .arg("-f")
        .arg(key.as_str())
        .stdin(text.as_bytes().to_vec())
}

/// Keeps `ssh-keygen`'s output only when it is an armored SSH signature.
fn armored(stdout: &[u8]) -> Result<Vec<u8>, String> {
    match SshSig::from_pem(stdout) {
        Ok(_) => Ok(stdout.to_vec()),
        Err(_) => Err("ssh-keygen wrote no SSH signature".to_owned()),
    }
}

/// The filter of the signing step.
pub const ARMORED: Filter = Filter {
    name: "an SSH signature",
    apply: armored,
};

/// The signed pair at `out` and `sig`, checked as the gate checks it, with `public` as
/// the only allowed signer.
///
/// # Errors
/// Either file is missing, or the pair would be refused by the gate.
pub fn check_pair<E: Exec>(
    exec: &E,
    request: &EraseRequest,
    public: &PublicKey,
    (out, sig): (&HostPath, &HostPath),
    now: i64,
) -> Result<Signer, CheckError> {
    let read = |path: &HostPath| match exec.inspect(path) {
        Ok(Some(Entry::File { bytes, .. })) => Ok(bytes),
        _ => Err(CheckError::Missing(path.clone())),
    };
    let text = read(out)?;
    check_text(&text, request)?;
    let signature =
        String::from_utf8(read(sig)?).map_err(|_| CheckError::NotText("the signature"))?;
    let line = format!(
        "{SELF} namespaces=\"{NAMESPACE}\" {}",
        public.to_openssh().unwrap_or_default()
    );
    // An empty set when the line does not parse: the check then finds no signer.
    let signers = AllowedSigners::parse(&line).unwrap_or_default();
    Ok(signers.verify(&text, &signature, now, false)?)
}

/// When the request is made: the time (seconds since the epoch) and the 16 random
/// bytes of its nonce.
#[derive(Clone, Copy, Debug)]
pub struct Made {
    pub now: i64,
    pub nonce: [u8; 16],
}

/// Plan the signature and the request for `a`, print the request and the plan, and in
/// [`Mode::Apply`] carry the plan out and check the pair.
///
/// # Errors
/// A refusal (exit 2), a failed signing step, or a pair that does not check (exit 1).
pub fn run<E: Exec>(
    exec: &mut E,
    a: &EraseArgs,
    made: Made,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), MdmError> {
    if !(1..=MAX_VALID_FOR).contains(&a.valid_for) {
        return Err(EraseError::ValidFor(a.valid_for).into());
    }
    let purpose = a
        .lease
        .clone()
        .map_or(Purpose::EraseNow, Purpose::PrivilegedLease);
    let request = EraseRequest {
        serial: a.serial.clone(),
        purpose,
        reason: a.reason.clone(),
        nonce: hex::encode(made.nonce),
        not_after: made.now + i64::from(a.valid_for) * 60,
    };
    let text = request::render(&request);
    check_text(text.as_bytes(), &request).map_err(EraseError::Request)?;
    let public = public_key(exec, &a.key)?;
    let sig = HostPath::new(&format!("{}.sig", a.out))?;
    for path in [&a.out, &sig] {
        if inspect(exec, path)?.is_some() {
            return Err(EraseError::Exists(path.clone()).into());
        }
    }
    let mut planner = Planner::new(&*exec);
    // The signature first: when the key is not touched, nothing is left behind.
    planner.capture(
        sign_cmd(&a.ssh_keygen, &a.key, &text),
        ARMORED,
        sig.clone(),
        Perms::new(0o644),
        None,
    );
    planner.file(
        a.out.clone(),
        Perms::new(0o644),
        None,
        text.clone().into_bytes(),
    )?;
    let plan = planner.finish();
    say(
        out,
        &format!(
            "erase request for {}, acceptable until {}:",
            request.serial,
            format_rfc3339(request.not_after)
        ),
    )?;
    for line in text.lines() {
        say(out, &format!("    {line}"))?;
    }
    if mode == Mode::Apply {
        say(
            out,
            "touch the security key when it blinks (and enter its PIN if asked)",
        )?;
    }
    execute(&plan, RunAs::User, mode, exec, &mut out)?;
    if mode == Mode::DryRun {
        return Ok(());
    }
    let pair = (&a.out, &sig);
    let signer =
        check_pair(&*exec, &request, &public, pair, made.now).map_err(|why| EraseError::Check {
            out: a.out.clone(),
            sig: sig.clone(),
            why,
        })?;
    let pin = if signer.flags & FLAG_USER_VERIFIED == 0 {
        "no PIN (a gate that requires one refuses it)"
    } else {
        "PIN verified"
    };
    say(
        out,
        &format!(
            "checked as the gate checks it: {}, touched, {pin}, counter {}",
            signer.algorithm, signer.counter
        ),
    )?;
    say(
        out,
        &format!(
            "send {} and {sig} to the gate before {}",
            a.out,
            format_rfc3339(request.not_after)
        ),
    )
}

fn say(out: &mut dyn Write, line: &str) -> Result<(), MdmError> {
    writeln!(out, "{line}").map_err(MdmError::Write)
}

/// 16 bytes from the kernel's random source, for a nonce.
///
/// # Errors
/// The random source cannot be read.
pub fn random_nonce() -> Result<[u8; 16], MdmError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| io::Read::read_exact(&mut f, &mut bytes))
        .map_err(MdmError::Random)?;
    Ok(bytes)
}

#[cfg(test)]
#[path = "erase_tests.rs"]
mod tests;
