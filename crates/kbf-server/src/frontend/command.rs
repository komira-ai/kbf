//! The command line of `kbf frontend` ([`command`]): the flags of [`Args`] it takes, the flags
//! of its own modules, and the retired handover flags, hidden. [`super::parse`] reads it back.

use std::path::PathBuf;

use clap::{Arg, Command};

use super::{
    ALLOWED, RetiredHandoverFlags, SECRETS_DIR, TLS, drain_flags, executor_flags, handoff, janitor,
    job_read, pack_flags, state_wait, task_flags,
};

/// The command line of `kbf frontend`: [`Args`]'s, those it takes shown and the rest
/// hidden (so [`parse`] can refuse them by name), `--storage` optional, and the three
/// flags toward storage (which need it).
#[must_use]
pub fn command() -> Command {
    let path =
        |id: &'static str, long: &'static str, requires: [&'static str; 2], help: &'static str| {
            Arg::new(id)
                .long(long)
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires_all(requires)
                .help(help)
        };
    crate::flag_set::command(
        "frontend",
        "The REAPI front end: schedules actions on daemons or as jobs, and keeps blobs on a \
         storage node (--storage) or in packs (--pack-store-endpoint, with no --storage).",
        ALLOWED,
    )
    .mut_arg("storage", |a| {
        a.help(
            "The storage node the frontend proxies blobs to, as `http://host:port` or \
             `https://host:port` with the three flags below. Left out, the frontend has no \
             storage node: it needs `--executor js-agent`, `--pack-index` and \
             `--pack-store-endpoint` (and its companions); client uploads are kept in packs \
             (and, over the pack bound, as `cas/l/` objects) in that object store, the action \
             cache is the `action_result` table of the Postgres state store, and `/readyz` asks \
             the state store and the job service and nothing else. Needs the installer's \
             action-cache migration",
        )
    })
    .arg(
        path(
            TLS[0],
            "storage-client-cert",
            [TLS[1], TLS[2]],
            "The frontend's certificate toward the storage node (PEM); it must name the role \
         `frontend` (a URI subjectAltName urn:kbf:role:frontend). Needs --storage.",
        )
        .requires("storage"),
    )
    .arg(
        path(
            TLS[1],
            "storage-client-key",
            [TLS[0], TLS[2]],
            "The private key of --storage-client-cert (PEM). Needs --storage.",
        )
        .requires("storage"),
    )
    .arg(
        path(
            TLS[2],
            "storage-ca",
            [TLS[0], TLS[1]],
            "The CA the storage node's certificate chains to; the node must present a \
         certificate for the host in the --storage URL. Needs --storage.",
        )
        .requires("storage"),
    )
    .args(crate::enroll::flags::args())
    .args(handoff::args())
    .args(drain_flags::args())
    .args(state_wait::args())
    .args(pack_flags::args())
    .args(job_read::args())
    .args(task_flags::args())
    .args(janitor::args())
    .args(executor_flags::args())
    .arg(
        Arg::new(SECRETS_DIR)
            .long("object-store-secrets-dir")
            .value_name("DIR")
            .value_parser(clap::value_parser!(PathBuf))
            .requires_all(["api_listen", "api_token_file", "state_store"])
            .help(
                "Turns on the object-store registration routes of the operator API \
                 (/v1/object-stores, docs/api.md) and names the directory where the secret \
                 key of each registered connection is kept, one file of mode 0600 each, in a \
                 directory made 0700. The directory must be readable by every frontend \
                 child that serves at once and left out of any backup of the host. The \
                 registration itself (endpoints, buckets, the audit log) is kept in the \
                 state store. Without it those routes answer 404.",
            ),
    )
    .args(retired_args())
}

/// The arguments of [`RetiredHandoverFlags`], hidden from `--help`, to add to the command.
fn retired_args() -> Vec<Arg> {
    let retired = <RetiredHandoverFlags as clap::Args>::augment_args(Command::new("retired"));
    retired.get_arguments().cloned().collect()
}
