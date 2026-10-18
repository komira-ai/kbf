//! What the environment gives the process: the S3 key pair, the directories apart, the
//! container driver's programs and runtime directory, the open-file limit.

use std::io;
use std::path::{Path, PathBuf};

use super::host::Kind;
use super::{CRUN_MINIMUM, Host, NOFILE_FLOOR, Needs, Report, files::class_of};

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    if needs.s3 {
        s3(host, report);
    }
    if let Some((meta, data)) = &needs.meta_and_data {
        meta_and_data(meta, data, report);
    }
    if let Some(container) = &needs.container {
        podman(host, report);
        runtime_dir(host, report);
        if let Some(crun) = &container.oci_runtime {
            self::crun(crun, host, report);
        }
    }
    nofile(host, report);
}

fn s3(host: &dyn Host, report: &mut Report) {
    for name in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] {
        let what = match host.var(name) {
            Some(v) if !v.is_empty() => continue,
            Some(_) => format!("{name} is empty"),
            None => format!("{name} is not set"),
        };
        report.refuse(
            what,
            "--cold-store is an s3:// store and kbf signs its requests with the key pair",
            format!(
                "set {name} in the unit's EnvironmentFile (a file only the service user can read)"
            ),
        );
    }
}

fn meta_and_data(meta: &Path, data: &Path, report: &mut Report) {
    if meta.starts_with(data) {
        report.refuse(
            format!(
                "--meta-dir {} is inside --data-dir {}",
                meta.display(),
                data.display()
            ),
            "the data directory is the daemon's cache and lease space, sized to the free space \
             of its file system; the metadata log must not live in it",
            "put --meta-dir on local disk outside --data-dir".to_owned(),
        );
    }
}

/// The first executable file called `name` on the PATH of `host`.
fn on_path(name: &str, host: &dyn Host) -> bool {
    host.var("PATH").unwrap_or_default().split(':').any(|dir| {
        let candidate: PathBuf = Path::new(if dir.is_empty() { "." } else { dir }).join(name);
        host.stat(&candidate)
            .is_ok_and(|s| s.kind == Kind::File && s.mode & 0o111 != 0)
    })
}

fn podman(host: &dyn Host, report: &mut Report) {
    if !on_path("podman", host) {
        report.refuse(
            format!(
                "podman is not on PATH ({})",
                host.var("PATH")
                    .unwrap_or_else(|| "PATH is not set".to_owned())
            ),
            "the container driver runs every action, and checks its images, through Podman",
            "install podman, or put its directory on the unit's PATH".to_owned(),
        );
    }
}

fn runtime_dir(host: &dyn Host, report: &mut Report) {
    const WHY: &str = "rootless Podman keeps its sockets and runtime state there";
    let me = host.euid();
    let linger = "run `loginctl enable-linger USER` for the user the unit runs as, so systemd \
                  makes /run/user/UID at boot";
    let Some(dir) = host.var("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        report.refuse(
            "XDG_RUNTIME_DIR is not set".to_owned(),
            WHY,
            format!("{linger}, and set XDG_RUNTIME_DIR=/run/user/{me} in the unit"),
        );
        return;
    };
    match host.stat(Path::new(&dir)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => report.refuse(
            format!("XDG_RUNTIME_DIR {dir} does not exist"),
            WHY,
            linger.to_owned(),
        ),
        Err(e) => report.push(
            class_of(&e),
            format!("XDG_RUNTIME_DIR {dir} cannot be read: {e}"),
            WHY,
            linger.to_owned(),
        ),
        Ok(s) if s.kind != Kind::Dir => report.refuse(
            format!("XDG_RUNTIME_DIR {dir} is not a directory"),
            WHY,
            linger.to_owned(),
        ),
        Ok(s) if s.uid != me => report.refuse(
            format!(
                "XDG_RUNTIME_DIR {dir} is owned by uid {}, not by uid {me}, the user kbf runs as",
                s.uid
            ),
            WHY,
            format!("set XDG_RUNTIME_DIR=/run/user/{me} in the unit"),
        ),
        Ok(_) => {}
    }
}

fn crun(path: &Path, host: &dyn Host, report: &mut Report) {
    const WHY: &str = "--container-runtime=oci runs every container with it";
    let at = path.display();
    let fix = "install crun (1.14 or newer), or point --oci-runtime at it".to_owned();
    match host.stat(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            report.refuse(format!("{at} (--oci-runtime) does not exist"), WHY, fix);
            return;
        }
        Err(e) => {
            report.push(
                class_of(&e),
                format!("{at} (--oci-runtime) cannot be read: {e}"),
                WHY,
                fix,
            );
            return;
        }
        Ok(s) if s.kind != Kind::File => {
            report.refuse(
                format!("{at} (--oci-runtime) is not a regular file"),
                WHY,
                fix,
            );
            return;
        }
        Ok(s) if s.mode & 0o111 == 0 => {
            report.refuse(
                format!("{at} (--oci-runtime) is not executable"),
                WHY,
                format!("chmod +x {at}"),
            );
            return;
        }
        Ok(_) => {}
    }
    let out = match host.run(path, &["--version"]) {
        Ok(out) if out.success => out,
        Ok(_) => {
            report.refuse(format!("{at} --version failed"), WHY, fix);
            return;
        }
        Err(e) => {
            report.refuse(format!("{at} --version failed: {e}"), WHY, fix);
            return;
        }
    };
    let first = out.stdout.lines().next().unwrap_or_default();
    let Some((major, minor)) = version(first) else {
        report.refuse(
            format!("{at} --version printed {first:?}, which is not a version"),
            WHY,
            fix,
        );
        return;
    };
    if (major, minor) < CRUN_MINIMUM {
        report.refuse(
            format!(
                "{at} is crun {major}.{minor}, older than {}.{}",
                CRUN_MINIMUM.0, CRUN_MINIMUM.1
            ),
            "the container driver reads the failures of crun 1.14 and newer",
            fix,
        );
    }
}

/// The major and minor number of `crun version 1.14.1`.
fn version(line: &str) -> Option<(u64, u64)> {
    let number = line.split_once("version")?.1.trim();
    let mut parts = number
        .split(|c: char| !c.is_ascii_digit())
        .map(str::parse::<u64>);
    let major = parts.next()?.ok()?;
    let minor = parts.next().and_then(Result::ok).unwrap_or(0);
    Some((major, minor))
}

fn nofile(host: &dyn Host, report: &mut Report) {
    let limit = host.nofile();
    if limit < NOFILE_FLOOR {
        report.warnings.push(format!(
            "the open-file limit is {limit}, below {NOFILE_FLOOR}: a burst of uploads fails \
             with EMFILE; set LimitNOFILE={NOFILE_FLOOR} (or more) in the unit"
        ));
    }
}
