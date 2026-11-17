//! What the environment gives the process: the S3 key pair and the open-file limit.

use super::{Host, NOFILE_FLOOR, Needs, Report};

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    if needs.s3 {
        s3(host, report);
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

fn nofile(host: &dyn Host, report: &mut Report) {
    let limit = host.nofile();
    if limit < NOFILE_FLOOR {
        report.warnings.push(format!(
            "the open-file limit is {limit}, below {NOFILE_FLOOR}: a burst of uploads fails \
             with EMFILE; set LimitNOFILE={NOFILE_FLOOR} (or more) in the unit"
        ));
    }
}
