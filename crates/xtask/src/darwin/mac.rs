//! The macOS half of [`super`]: the runs of `codesign`, `vtool`, `csrutil` and the
//! binary itself.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{cdhash, code_directory_flags, granted, minos, sip_enabled};
use crate::Error;
use crate::entitlements::{self, AllowList};

/// The code directory flags of an ad-hoc signature with the hardened runtime.
const FLAGS: &str = "adhoc,runtime";

/// A directory of this call alone, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(what: &str) -> Result<Scratch, Error> {
        // Tests call the verbs from several threads of one process.
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("xtask-{what}-{pid}-{call}"));
        // Absent unless an earlier process with this pid left it.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `program args...`, finished, with its output captured.
fn output<I, S>(program: &str, args: I) -> Result<Output, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(program)
        .args(args)
        .output()
        .map_err(Error::spawn(program.to_owned()))
}

/// Stdout and stderr of `out`, one after the other.
fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `program args...` must exit 0; its output is in the error when it does not.
fn succeeds<I, S>(program: &str, args: I, what: &str) -> Result<String, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = output(program, args)?;
    if !out.status.success() {
        return Err(Error::Failed(format!("{what}: {}", text(&out).trim_end())));
    }
    Ok(text(&out))
}

/// The binary's file name, as the allow list names binaries.
fn file_name(binary: &Path) -> Result<&str, Error> {
    binary
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| Error::Failed(format!("{} has no file name", binary.display())))
}

/// Check 1: `codesign --verify --strict` accepts the signature.
fn verify(binary: &Path) -> Result<(), Error> {
    let args = [
        OsStr::new("--verify"),
        "--strict".as_ref(),
        "--verbose=2".as_ref(),
    ];
    succeeds(
        "codesign",
        args.into_iter().chain([binary.as_os_str()]),
        &format!("codesign --verify --strict refused {}", binary.display()),
    )?;
    Ok(())
}

/// `codesign --display --verbose=4` of the binary, and its code directory flags,
/// which must be exactly [`FLAGS`] (check 3, first half).
fn display(binary: &Path) -> Result<(String, String), Error> {
    let info = text(&output(
        "codesign",
        [
            OsStr::new("--display"),
            "--verbose=4".as_ref(),
            binary.as_os_str(),
        ],
    )?);
    let flags = code_directory_flags(&info).unwrap_or_default().to_owned();
    if flags != FLAGS {
        return Err(Error::Failed(format!(
            "code directory flags are '{flags}', want '{FLAGS}'\n{info}"
        )));
    }
    Ok((info, flags))
}

/// Check 2: `<binary> --version` exits 0 with only `PATH` and `HOME` set.
fn runs(binary: &Path, what: &str) -> Result<(), Error> {
    let mut cmd = Command::new(binary);
    cmd.arg("--version").env_clear();
    for var in ["PATH", "HOME"] {
        if let Some(v) = std::env::var_os(var) {
            cmd.env(var, v);
        }
    }
    let out = cmd
        .output()
        .map_err(Error::spawn(binary.display().to_string()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "{what} did not run: '{} --version' failed: {}",
            binary.display(),
            text(&out).trim_end()
        )));
    }
    Ok(())
}

/// Whether `<binary> --version` exits 0 with `DYLD_INSERT_LIBRARIES` naming `missing`.
fn starts_with_inserted(binary: &Path, missing: &Path) -> Result<bool, Error> {
    let out = Command::new(binary)
        .arg("--version")
        .env("DYLD_INSERT_LIBRARIES", missing)
        .stdout(Stdio::null())
        .output()
        .map_err(Error::spawn(binary.display().to_string()))?;
    Ok(out.status.success())
}

/// The entitlements the signature of `binary` carries, as `codesign --display
/// --entitlements - --xml` prints them: empty for none.
fn signed_entitlements(binary: &Path) -> Result<String, Error> {
    if !binary.is_file() {
        return Err(Error::Failed(format!(
            "{} does not exist",
            binary.display()
        )));
    }
    let args = [
        OsStr::new("--display"),
        "--entitlements".as_ref(),
        "-".as_ref(),
        "--xml".as_ref(),
        binary.as_os_str(),
    ];
    let out = output("codesign", args)?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "codesign cannot read the signature of {}: {}",
            binary.display(),
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Check 3, second half: the signature of `binary` carries exactly the entitlements the
/// list at `allowlist` gives `name` ([`entitlements::compare`]). The list is read
/// first, so a missing or malformed list fails before the binary is looked at.
pub fn check_entitlements(binary: &Path, name: &str, allowlist: &Path) -> Result<String, Error> {
    let list = AllowList::read(allowlist)?;
    let signed = signed_entitlements(binary)?;
    Ok(entitlements::compare(&list, name, &signed)?)
}

/// The binary's minimum macOS, from `vtool -show-build`.
pub fn minos_of(binary: &Path) -> Result<String, Error> {
    let build = succeeds(
        "vtool",
        [OsStr::new("-show-build"), binary.as_os_str()],
        &format!("vtool -show-build {}", binary.display()),
    )?;
    minos(&build)
        .map(str::to_owned)
        .ok_or_else(|| Error::Failed(format!("vtool names no minos:\n{build}")))
}

/// The `sign-darwin` verb (module docs): signs `binary` in place with the entitlements
/// the list at `allowlist` gives its file name, and checks the result. Appends a line
/// to `summary` when given. The lines to print.
pub fn sign(
    binary: &Path,
    allowlist: &Path,
    want_minos: &str,
    summary: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let name = file_name(binary)?;
    // Fails, and so stops here, if the allow list is missing or malformed.
    let list = AllowList::read(allowlist)?;
    let scratch = Scratch::new("sign-darwin")?;
    let missing = scratch.0.join("kbf-no-such-library.dylib");
    let mut lines = Vec::new();

    // 5, the control: the binary as the linker signed it (ad hoc, no hardened runtime).
    let unhardened = scratch.0.join(format!("unhardened-{name}"));
    std::fs::copy(binary, &unhardened)
        .map_err(Error::spawn(format!("copy {}", binary.display())))?;
    if starts_with_inserted(&unhardened, &missing)? {
        return Err(Error::Failed(
            "control failed: the unhardened binary started with DYLD_INSERT_LIBRARIES set, \
             so this host cannot show whether the hardened runtime is in force"
                .to_owned(),
        ));
    }
    lines.push(
        "control: dyld refused the unhardened binary with a missing inserted library".to_owned(),
    );

    let mut args: Vec<&OsStr> = ["--force", "--sign", "-", "--options", "runtime"]
        .map(OsStr::new)
        .to_vec();
    let plist_file = scratch.0.join(format!("entitlements-{name}.plist"));
    let keys = list.entitlements(name);
    if let Some(plist) = list.plist(name) {
        std::fs::write(&plist_file, &plist)
            .map_err(Error::spawn(format!("write {}", plist_file.display())))?;
        lines.push(format!(
            "signing {name} with the entitlements the allow list gives it:"
        ));
        lines.extend(plist.lines().map(str::to_owned));
        args.extend([OsStr::new("--entitlements"), plist_file.as_os_str()]);
    }
    args.push(binary.as_os_str());
    succeeds(
        "codesign",
        args,
        &format!("codesign could not sign {}", binary.display()),
    )?;

    verify(binary)?;
    runs(binary, "the signed binary")?;
    let (info, flags) = display(binary)?;
    lines.extend(info.lines().map(str::to_owned));
    lines.push(check_entitlements(binary, name, allowlist)?);

    // 4
    let got_minos = minos_of(binary)?;
    if got_minos != want_minos {
        return Err(Error::Failed(format!(
            "minos is '{got_minos}', want '{want_minos}'"
        )));
    }

    // 5
    let sip = output("csrutil", ["status"]).map_or_else(|e| e.to_string(), |o| text(&o));
    lines.push(sip.trim_end().to_owned());
    if starts_with_inserted(binary, &missing)? {
        lines.push(
            "hardened: the signed binary started and ignored DYLD_INSERT_LIBRARIES".to_owned(),
        );
    } else if !sip_enabled(&sip) {
        lines.push(format!(
            "::warning::not checked in behaviour: SIP is not enabled here ({}), and dyld \
             honoured DYLD_INSERT_LIBRARIES for the hardened binary",
            sip.trim_end()
        ));
    } else {
        return Err(Error::Failed(
            "the signed binary honoured DYLD_INSERT_LIBRARIES with SIP enabled".to_owned(),
        ));
    }

    let cdhash = cdhash(&info).unwrap_or_default();
    let granted = granted(keys);
    lines.push(format!(
        "signed {}: flags {flags}, entitlements {granted}, cdhash {cdhash}",
        binary.display()
    ));
    if let Some(path) = summary {
        let line = format!(
            "`{name}` (darwin-arm64): ad-hoc signed, flags `{flags}`, entitlements {granted}, \
             cdhash `{cdhash}`, minos {got_minos}\n"
        );
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .map_err(Error::spawn(format!("append to {}", path.display())))?;
    }
    Ok(lines)
}

/// The `darwin-asset` verb (module docs): the binary `name` read out of `tarball` and
/// checked as it ships. The lines to print.
pub fn asset(tarball: &Path, name: &str, allowlist: &Path) -> Result<Vec<String>, Error> {
    if Path::new(name).file_name() != Some(OsStr::new(name)) {
        return Err(Error::Failed(format!("{name:?} is not a file name")));
    }
    let tgz = crate::package::read(tarball)?;
    let bytes = binary_in(&tgz, name)
        .ok_or_else(|| Error::Failed(format!("{} holds no {name}", tarball.display())))?;
    let problems = crate::package::check_tarball(&tgz, name, &bytes);
    if !problems.is_empty() {
        return Err(Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", tarball.display()))
                .collect(),
        ));
    }
    let scratch = Scratch::new("darwin-asset")?;
    let binary = scratch.0.join(name);
    write_executable(&binary, &bytes)?;

    verify(&binary)?;
    let (info, flags) = display(&binary)?;
    runs(&binary, "the packaged binary")?;
    let mut lines = vec![check_entitlements(&binary, name, allowlist)?];
    let shown = tarball
        .file_name()
        .unwrap_or(tarball.as_os_str())
        .to_string_lossy();
    lines.push(format!(
        "checked {shown}: signature valid, flags {flags}, cdhash {}",
        cdhash(&info).unwrap_or_default()
    ));
    Ok(lines)
}

/// The bytes of the first entry named `name` in the gzip-compressed tar `tgz`.
fn binary_in(tgz: &[u8], name: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        if *entry.path_bytes() == *name.as_bytes() {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).ok()?;
            return Some(bytes);
        }
    }
    None
}

/// Writes `bytes` to the new file `path`, mode 0755.
fn write_executable(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(Error::spawn(format!("write {}", path.display())))
}
