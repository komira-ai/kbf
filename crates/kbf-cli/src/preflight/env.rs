//! What the environment gives the process: the S3 key pair, the
//! container driver's runtime (`crun`), the open-file limit.

use std::io;
use std::path::Path;

use super::host::Kind;
use super::{CRUN_MINIMUM, Host, NOFILE_FLOOR, Needs, Report, files::class_of};

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    if needs.s3 {
        s3(host, report);
    }
    if let Some(container) = &needs.container {
        crun(&container.oci_runtime, host, report);
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

fn crun(path: &Path, host: &dyn Host, report: &mut Report) {
    const WHY: &str = "the container driver runs every container with it";
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
