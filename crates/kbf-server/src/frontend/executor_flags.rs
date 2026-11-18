//! The flags of the executor: `--executor` and its companions.
//!
//! `--executor js-agent` runs each Execute as a job of the job scheduler's job
//! service ([`crate::executor`]); it is the only executor there is. Without `--executor` no
//! executor is built, no job service is dialled and none of the companions is read: the
//! frontend serves its cache and CAS alone. `--executor kbf-daemon`, which named the deleted
//! worker, is refused at start, and any other value too.
//!
//! The companions are the frontend's own. They are not refused without `--executor`, so a
//! `kbf.toml` that names the job service starts with or without the executor.
//!
//! Every Execute is one job of the job service, submitted at once and read back every
//! `--executor-poll-ms` ([`crate::executor::Executor`]): there are no virtual workers, no slots
//! and no ownership modes. Over a `--state-store` each frontend keeps a row for every operation
//! and a liveness row that lives `--executor-live-secs`; once a frontend's row has expired,
//! another frontend of the store takes up its unfinished operations. Every frontend of one farm
//! gives the same `--executor-frontend-id`.
//!
//! Seven flags of the virtual workers and their ownership (`--executor-hosts`,
//! `--executor-host-cpus`, `--executor-host-memory-gib`, `--executor-host-slots`,
//! `--executor-prepare-concurrency`, `--executor-ownership`, `--executor-mac-hosts`) are
//! retired: they parse with a value, are hidden from `--help`, are read by nothing, and each
//! one given logs one warning at start ([`ExecutorFlags::warn_retired`]), so a `kbf.toml` or a
//! unit written before the removal still starts.
//!
//! `--executor-input-tree` chooses what a Linux put-directory job's root is read from: the
//! action tree's manifests under tierstore's trees (`manifests`, the default), or one tierstore
//! tree list that tierstore's mount provider makes the tree from (`provider`,
//! [`crate::executor::InputTree`]).
//!
//! The executor speaks to a job service only. The flags of the lease server it once leased
//! hosts from (`--executor-lease-url`, `--executor-lease-token-file`,
//! `--executor-lease-ca-file`, `--executor-lease-ttl-secs`, `--executor-class`) are gone, not
//! aliased: a frontend started with one fails at start, naming it, and never runs on the wrong
//! protocol.
//!
//! A job books no disk: the pool books none, and a job writes to its host's scratch with no
//! quota. `--executor-output-disk-bytes`, the scratch a job once asked for its outputs, is gone
//! the same way: a frontend started with it fails at start, naming it.
//!
//! A put-directory job is given no secret and no network, so the flags of the executor's
//! presigned path were removed with it: `--executor-secret-delivery`,
//! `--executor-insecure-argv-secrets`, `--executor-work-dir`, `--executor-allow-plain-http-put`,
//! `--executor-allow-plain-http-grant`, `--executor-locate`, `--executor-network` and
//! `--executor-allow-open-network`. Each still exists as a hidden flag that refuses any value
//! at start, naming the release that removed it, so that a `kbf.toml` copied from an older
//! template is told to delete the line and not "unknown key".
//!
//! Every secret is a file: the job service's token (mode 0600 or 0400) and its CA certificate.

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::builder::ValueParser;
use clap::{Arg, ArgMatches};
use kbf_front::action_tree::{BaseTree, DEFAULT_TOTAL, MANIFEST_WRITES, PutRetry};
use kbf_front::{Backend, Packs, Recorder};
use kbf_job_client::{Client, Origin, Token};
use kbf_types::ImageRef;

use crate::executor::{
    Config, DEFAULT_MAC_OS_VERSION, Deps, Executor, Front, FrontParts, InputTree, MacPool, Metrics,
    PutDirParts, PutDirRoots, RunSource,
};

const KIND: &str = "executor";
const FRONTEND: &str = "executor_frontend_id";
const JOB_URL: &str = "executor_job_url";
const TOKEN_FILE: &str = "executor_job_token_file";
const CA_FILE: &str = "executor_job_ca_file";
const ARCH: &str = "executor_arch";
const BASE_IMAGE: &str = "executor_base_image";
const KBF_IMAGE: &str = "executor_kbf_image";
const BASE_TREE: &str = "executor_base_tree";
const BASE_TREE_URL: &str = "executor_base_tree_url";
const BASE_TREE_KEEP: &str = "executor_base_tree_keep";
const BASE_TREE_TIMEOUT: &str = "executor_base_tree_timeout_secs";
/// The default of `--executor-base-tree-url`: the releases of kbf's own repository.
pub(super) const DEFAULT_BASE_TREE_URL: &str =
    "https://api.github.com/repos/komira-ai/kbf/releases";
const MARGIN: &str = "executor_run_margin_secs";
const FAILED_STDERR_LOG: &str = "executor_failed_stderr_log_bytes";
const MAX_RUN: &str = "executor_max_run_secs";
const DEFAULT_TIMEOUT: &str = "executor_default_timeout_secs";
const CALL_TIMEOUT: &str = "executor_call_timeout_secs";
const POLL: &str = "executor_poll_ms";
const PREPARE_LIMIT: &str = "executor_prepare_limit_secs";
const INPUT_TREE: &str = "executor_input_tree";
const LIVE: &str = "executor_live_secs";
const MAC_BUNDLE: &str = "executor_mac_task_bundle";
const MAC_OS_VERSION: &str = "executor_mac_os_version";
const MAC_OUT_ROOT: &str = "executor_mac_task_out_root";

/// The retired flags: argument id, flag and value name. Values are not checked.
const RETIRED: [(&str, &str, &str); 7] = [
    ("executor_hosts", "--executor-hosts", "WORKERS"),
    ("executor_host_cpus", "--executor-host-cpus", "CORES"),
    (
        "executor_host_memory_gib",
        "--executor-host-memory-gib",
        "GIB",
    ),
    ("executor_host_slots", "--executor-host-slots", "RUNS"),
    (
        "executor_prepare_concurrency",
        "--executor-prepare-concurrency",
        "COUNT",
    ),
    ("executor_ownership", "--executor-ownership", "MODE"),
    ("executor_mac_hosts", "--executor-mac-hosts", "WORKERS"),
];

/// The default of `--executor-live-secs` as clap takes it, a literal; the default-flags test
/// checks it against [`LIVE`].
const DEFAULT_LIVE_SECS: &str = "20";

/// The default of `--executor-poll-ms` as clap takes it.
const DEFAULT_POLL_MS: &str = "250";

/// The flags a `js-agent` executor cannot do without, in the order their absence is named.
const REQUIRED: [(&str, &str); 6] = [
    (JOB_URL, "--executor-job-url"),
    (TOKEN_FILE, "--executor-job-token-file"),
    (FRONTEND, "--executor-frontend-id"),
    (BASE_IMAGE, "--executor-base-image"),
    (KBF_IMAGE, "--executor-kbf-image"),
    (BASE_TREE, "--executor-base-tree"),
];

fn text(id: &'static str, long: &'static str, value_name: &'static str, help: &'static str) -> Arg {
    Arg::new(id)
        .long(long)
        .value_name(value_name)
        .value_parser(clap::value_parser!(String))
        .help(help)
}

fn path(id: &'static str, long: &'static str, help: &'static str) -> Arg {
    Arg::new(id)
        .long(long)
        .value_name("PATH")
        .value_parser(clap::value_parser!(PathBuf))
        .help(help)
}

fn number(id: &'static str, long: &'static str, help: &'static str, default: &'static str) -> Arg {
    Arg::new(id)
        .long(long)
        .value_name("SECONDS")
        .value_parser(clap::value_parser!(u64).range(1..))
        .default_value(default)
        .help(help)
}

/// The flags of the presigned path that was removed, by long name: they only refuse.
pub(super) const REMOVED: [&str; 8] = [
    "executor-secret-delivery",
    "executor-insecure-argv-secrets",
    "executor-work-dir",
    "executor-allow-plain-http-put",
    "executor-allow-plain-http-grant",
    "executor-locate",
    "executor-network",
    "executor-allow-open-network",
];

/// What a start that gives one of the removed flags is told, after clap's `invalid value ... for
/// '--<flag> <VALUE>'`, which names the flag.
pub(super) const REMOVED_WHY: &str = concat!(
    "the flag was removed in this release (kbf-server ",
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("KBF_BUILD_COMMIT"),
    ") with the executor's presigned path, the only code that read it: a put-directory job is \
     given no secret and no network; delete the flag",
);

/// The removed flags, each refusing any value with [`REMOVED_WHY`], hidden from `--help`.
fn removed() -> impl Iterator<Item = Arg> {
    REMOVED.into_iter().map(|long| {
        Arg::new(long)
            .long(long)
            .hide(true)
            .value_name("VALUE")
            .help("Removed with the executor's presigned path. Any value is refused at start")
            .value_parser(ValueParser::new(refuse))
    })
}

/// The parser of a removed flag: it takes no value.
fn refuse(_: &str) -> Result<String, &'static str> {
    Err(REMOVED_WHY)
}

/// The flags.
pub(super) fn args() -> Vec<Arg> {
    let mut args = vec![
        Arg::new(KIND)
            .long("executor")
            .value_name("KIND")
            .value_parser(clap::value_parser!(String))
            .help(
                "Where the frontend runs the actions it is asked to execute: `js-agent`, the \
                 only value, runs each Execute as a job of the job scheduler's job service, in \
                 the kbf image (`--executor-kbf-image`, built from `--executor-base-image`), \
                 whose task tool runs the action inside it; it needs the job service's flags \
                 (`--executor-job-url`, `--executor-job-token-file`, and \
                 `--executor-job-ca-file` for https), `--executor-frontend-id`, the release's \
                 base tree bundles (`--executor-base-tree`), the packs \
                 (`--pack-store-endpoint`) and the put directories of the runs \
                 (`--task-secret-file`, `--task-out-root`). Left out, no executor is built and \
                 no job service is contacted, whatever the other `--executor-*` flags say",
            ),
        text(
            FRONTEND,
            "executor-frontend-id",
            "ID",
            "The name every frontend of the farm gives: the value of the `kbf-frontend` label \
             of every job, hashed into the job ids. 1 to 63 of letters, digits, `-`, `_` and \
             `.`. It is the same for every frontend of one farm, not unique to each. Used only \
             with `--executor js-agent`",
        ),
        text(
            JOB_URL,
            "executor-job-url",
            "URL",
            "The job service, as `https://host[:port]`, or `http://` to a loopback host. Used \
             only with `--executor js-agent`",
        ),
        path(
            TOKEN_FILE,
            "executor-job-token-file",
            "A file with the bearer token of the job service, readable by its owner alone (mode \
             0600 or 0400). The secret is never a flag: a command line is world-readable. Used \
             only with `--executor js-agent`",
        ),
        path(
            CA_FILE,
            "executor-job-ca-file",
            "The CA certificate (PEM) the job service's certificate chains to: the only trust \
             anchor, no system roots are consulted. Needed for an `https` job service and \
             refused for an `http` one. Used only with `--executor js-agent`",
        ),
        Arg::new(ARCH)
            .long("executor-arch")
            .value_name("ARCH")
            .value_parser(["x86_64", "arm64"])
            .default_value("x86_64")
            .help(
                "The CPU architecture of the Linux hosts. Every job of a Linux action asks for \
                 a host of `os=linux`, this `arch` and containers (the Macs' are `arm64`). Used \
                 only with `--executor js-agent`",
            ),
        text(
            BASE_IMAGE,
            "executor-base-image",
            "IMAGE",
            "The base environment the Command of an action sees, pinned by digest as \
             `<repository>@sha256:<64 hex digits>` (a leading `docker://` is optional). It is \
             the one image an action's `container-image` platform property may name: an action \
             that names another is refused as invalid, one that names none gets this. A tag is \
             refused. Needed with `--executor js-agent`",
        ),
        text(
            KBF_IMAGE,
            "executor-kbf-image",
            "IMAGE",
            "The kbf image, pinned by digest like `--executor-base-image` and built from it, \
             that every job is launched from. It keeps the task tool as its `ENTRYPOINT`, so \
             the job's arguments are the tool's flags and the action runs inside this image. \
             Must be another image than the base. Needed with `--executor js-agent`",
        ),
        Arg::new(BASE_TREE)
            .long("executor-base-tree")
            .value_name("DIR")
            .value_parser(clap::value_parser!(PathBuf))
            .help(
                "The frontend's cache of the release's kbf base tree: a directory the frontend \
                 may write (the user the frontend runs as). The release's `SHA256SUMS` and its \
                 `kbf-base-tree-<commit>-linux-<arch>.tree.tar` bundle of `--executor-arch` are \
                 kept in `<DIR>/<commit>/`; when `<DIR>` holds neither that nor the same two \
                 files loose (copied by hand, as an earlier release asked), the frontend \
                 fetches them from the release (`--executor-base-tree-url`), checks the bundle \
                 against the release's `SHA256SUMS` and only then puts them in the cache, \
                 keeping the newest older releases for a rollback \
                 (`--executor-base-tree-keep`). It then writes the objects of the kbf base tree \
                 that the pack store lacks to `cas/l/`, create-only, so the pack store \
                 credentials must be allowed to write there, and records the tree as \
                 `job-tree/<digest>` in `--state-store` if there is one. A bundle that cannot be \
                 had, is not listed or not the listed bytes, or a write that fails, fails the \
                 start, naming this flag. Needed with `--executor js-agent`; without it \
                 nothing is read, fetched or written",
            ),
        text(
            BASE_TREE_URL,
            "executor-base-tree-url",
            "URL",
            "Where the release's base tree files are looked up when `--executor-base-tree` \
             lacks them: a list of releases in the shape of GitHub's `GET \
             /repos/<owner>/<repo>/releases` (an array, each release with `assets` of `name` \
             and `browser_download_url`), read a page at a time (`?per_page=100&page=N`) until a \
             release lists the bundle and the `SHA256SUMS`. https, or http on loopback. Empty \
             fetches nothing: the files must then be in `--executor-base-tree` already. Used \
             only with `--executor js-agent`",
        )
        .default_value(DEFAULT_BASE_TREE_URL),
        Arg::new(BASE_TREE_KEEP)
            .long("executor-base-tree-keep")
            .value_name("RELEASES")
            .value_parser(clap::value_parser!(usize))
            .default_value("1")
            .help(
                "How many releases besides the running one stay in the `--executor-base-tree` \
                 cache after a fetch, for a rollback of the frontend: the newest fetched. 0 \
                 keeps none. Used only with `--executor js-agent`",
            ),
        number(
            BASE_TREE_TIMEOUT,
            "executor-base-tree-timeout-secs",
            "How long the fetch of the base tree files waits, in seconds, for a connection, for \
             the head of an answer and between two chunks of a body (a stall, not the whole \
             download). Used only with `--executor js-agent`",
            "60",
        ),
        number(
            MARGIN,
            "executor-run-margin-secs",
            "What a job's wall limit adds to the action's timeout, in seconds: the fetch of \
             the inputs and the upload of the pack. An action that fetches or uploads for \
             longer than this ends at the wall limit and is answered to the caller as a \
             timeout. Used only with `--executor js-agent`",
            "300",
        ),
        Arg::new(FAILED_STDERR_LOG)
            .long("executor-failed-stderr-log-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(0..=8192))
            .default_value("512")
            .help(
                "The most bytes of a failed job's standard error the frontend writes to its \
                 log, at WARN: the end of the stream. 0 writes \
                 none. The whole output (up to 8 KiB with standard output) goes with the result \
                 either way. Used only with `--executor js-agent`",
            ),
        number(
            MAX_RUN,
            "executor-max-run-secs",
            "The longest action timeout the executor runs, in seconds; an action that asks for \
             more is refused. Used only with `--executor js-agent`",
            "3000",
        ),
        number(
            DEFAULT_TIMEOUT,
            "executor-default-timeout-secs",
            "The timeout, in seconds, of an action that names none: at most \
             `--executor-max-run-secs`. Used only with `--executor js-agent`",
            "3000",
        ),
        number(
            CALL_TIMEOUT,
            "executor-call-timeout-secs",
            "How long one call to the job service may take before it counts as unanswered, in \
             seconds. Used only with `--executor js-agent`",
            "5",
        ),
        Arg::new(POLL)
            .long("executor-poll-ms")
            .value_name("MILLISECONDS")
            .value_parser(clap::value_parser!(u64).range(100..=10_000))
            .default_value(DEFAULT_POLL_MS)
            .help(
                "How often each operation's job is read back from the job service, and a \
                 submit whose answer was lost is sent again, in milliseconds: 100 to 10000. \
                 Used only with `--executor js-agent`",
            ),
        number(
            PREPARE_LIMIT,
            "executor-prepare-limit-secs",
            "How long the inputs of one run (its action, its input tree published into the \
             store, its keys) may take to prepare, in seconds; a run whose preparation takes \
             longer ends as an infrastructure failure. Used only with `--executor js-agent`",
            "300",
        ),
        Arg::new(INPUT_TREE)
            .long("executor-input-tree")
            .value_name("MODE")
            .value_parser(["manifests", "provider"])
            .default_value("manifests")
            .help(
                "What a Linux put-directory job's root is read from. `manifests`, the default: \
                 the action tree's manifests, the job's layer `<mp>/trees/<digest>`. \
                 `provider`: one tree list of the tree, the job's layer tierstore's mount \
                 provider, which fetches the tree's files while the job's sandbox is made. \
                 `provider` needs `--task-out-mode put-dir`. A Mac job's root is always \
                 `manifests`. Used only with `--executor js-agent`",
            ),
        number(
            LIVE,
            "executor-live-secs",
            "How long the liveness row of this process lives in the state store after its \
             last renewal, on the store's clock, in seconds. Once it has expired, another \
             frontend of the store takes up this process's unfinished operations. Read only \
             with `--state-store`. Used only with `--executor js-agent`",
            DEFAULT_LIVE_SECS,
        ),
        text(
            MAC_BUNDLE,
            "executor-mac-task-bundle",
            "NAME@sha256:HEX",
            "The release's macOS `kbf-task` bundle, by its manifest digest (the one entry of \
             the `index.json` of `kbf-task-bundle-<commit>-darwin-arm64.tar.gz`): the image of \
             a Mac job, whose entrypoint `kbf-task` runs. It enables the Macs: without it no \
             action whose platform asks for macOS is run, and `--executor-mac-os-version` \
             and `--executor-mac-task-out-root` are refused. Used only with `--executor \
             js-agent`",
        ),
        Arg::new(MAC_OS_VERSION)
            .long("executor-mac-os-version")
            .value_name("MAJOR")
            .value_parser(clap::value_parser!(u32).range(1..))
            // DEFAULT_MAC_OS_VERSION.
            .default_value("26")
            .help(
                "The least major macOS every Mac of the job service runs. An action's \
                 `os_version` is the least it takes (`26` means 26 or newer), and one that asks \
                 for a newer macOS than this is refused, naming both: the job service cannot be \
                 asked for a version. Used only with `--executor-mac-task-bundle`",
            ),
        text(
            MAC_OUT_ROOT,
            "executor-mac-task-out-root",
            "PATH",
            "The tier store's put directory on the Macs, if it is not `--task-out-root`: a Mac \
             job mounts its put directory under it and its action tree from the `trees` \
             directory beside it. Used only with `--executor-mac-task-bundle`, refused without \
             it",
        ),
    ];
    args.extend(RETIRED.into_iter().map(|(id, long, value)| {
        Arg::new(id)
            .long(&long[2..])
            .value_name(value)
            .hide(true)
            .help("Retired: no longer read, every Execute is a job of the job service")
    }));
    args.extend(removed());
    args
}

/// One line for each retired flag `m` was given, naming it.
fn retired_warnings(m: &ArgMatches) -> Vec<String> {
    RETIRED
        .into_iter()
        .filter(|(id, _, _)| m.contains_id(id))
        .map(|(_, long, _)| {
            format!("{long} is no longer read: every Execute is a job of the job service")
        })
        .collect()
}

/// What `--executor js-agent` and its companions say.
#[derive(Clone, Debug)]
pub struct JsAgentFlags {
    /// The executor's settings.
    pub config: Config,
    /// `--executor-job-url`.
    pub job_url: String,
    /// `--executor-job-token-file`.
    pub token_file: PathBuf,
    /// `--executor-job-ca-file`.
    pub ca_file: Option<PathBuf>,
    /// `--executor-base-image`, by digest.
    pub base_image: ImageRef,
    /// `--executor-kbf-image`, by digest.
    pub kbf_image: ImageRef,
    /// `--executor-base-tree`: the directory of the release's `SHA256SUMS` and base tree
    /// bundles.
    pub base_tree: PathBuf,
    /// `--executor-base-tree-url`: the releases list the bundle is fetched from; empty fetches
    /// nothing.
    pub base_tree_url: String,
    /// `--executor-base-tree-keep`: the older releases the cache keeps.
    pub base_tree_keep: usize,
    /// `--executor-base-tree-timeout-secs`.
    pub base_tree_timeout: Duration,
    /// Where the base tree published from [`JsAgentFlags::base_tree`] is set when the frontend
    /// starts: what the executor writes its action trees over (`--task-out-mode put-dir`).
    pub published: Arc<OnceLock<BaseTree>>,
    /// `--task-put-retry-secs`: how long the waits for a 503 of the store add up to when the
    /// executor writes an action tree. Set by the task flags; the default until then.
    pub put_retry: Duration,
    /// `--task-manifest-writes`: the most manifest writes of action trees open at once. Set by
    /// the task flags; the default until then.
    pub manifest_writes: usize,
}

/// What `--executor` says: jobs of a job service, or no executor.
#[derive(Clone, Debug, Default)]
pub struct ExecutorFlags {
    /// The settings of `js-agent`; `None` when `--executor` is left out.
    pub js: Option<JsAgentFlags>,
    /// One line for each retired flag that was given, naming it.
    pub retired: Vec<String>,
}

impl ExecutorFlags {
    /// Logs a warning for each retired flag that was given: call it once, at start.
    pub fn warn_retired(&self) {
        for line in &self.retired {
            tracing::warn!("{line}");
        }
    }
}

/// The setting the flags in `m` make.
///
/// # Errors
/// An `--executor` other than `js-agent` (`kbf-daemon` is named as deleted). For `js-agent`, a
/// flag it needs that is missing, a setting that breaks a rule ([`Config::validate`]), a job
/// service URL that is not one or does not agree with the CA flag, or a base or kbf image that
/// is not pinned by digest or is the same image twice.
pub(super) fn from_matches(m: &ArgMatches) -> Result<ExecutorFlags, String> {
    let retired = retired_warnings(m);
    match m.get_one::<String>(KIND).map(String::as_str) {
        Some("js-agent") => {}
        Some("kbf-daemon") => {
            return Err(
                "--executor kbf-daemon is gone: kbf-daemon was deleted, and placed actions run \
                 as jobs of the job service, with --executor js-agent"
                    .to_owned(),
            );
        }
        Some(other) => {
            return Err(format!(
                "--executor {other}: the only executor is js-agent, which runs placed actions \
                 as jobs of the job service"
            ));
        }
        None => {
            // Every setting of the executor may stay in `kbf.toml` without it: nothing reads it
            // here.
            return Ok(ExecutorFlags { js: None, retired });
        }
    }
    let missing: Vec<&str> = REQUIRED
        .iter()
        .filter(|(id, _)| !m.contains_id(id))
        .map(|(_, long)| *long)
        .collect();
    if !missing.is_empty() {
        return Err(format!("--executor js-agent needs {}", missing.join(", ")));
    }
    // The settings are judged together ([`Config::validate`]) once the flags beside these have
    // set what follows from them, the outputs' put directory among them (`executor_bind`).
    let js = js_agent_flags(m)?;
    Ok(ExecutorFlags {
        js: Some(js),
        retired,
    })
}

fn js_agent_flags(m: &ArgMatches) -> Result<JsAgentFlags, String> {
    let one = |id: &str| m.get_one::<String>(id).cloned().unwrap_or_default();
    let count = |id: &str| m.get_one::<u64>(id).copied().unwrap_or_default();
    let secs = |id: &str| Duration::from_secs(count(id));
    let job_url = one(JOB_URL);
    let origin = Origin::from_str(&job_url).map_err(|e| format!("--executor-job-url: {e}"))?;
    let ca_file = m.get_one::<PathBuf>(CA_FILE).cloned();
    match (origin.is_tls(), ca_file.is_some()) {
        (true, false) => {
            return Err("--executor-job-url is https: give --executor-job-ca-file".to_owned());
        }
        (false, true) => {
            return Err("--executor-job-ca-file is for an https --executor-job-url".to_owned());
        }
        _ => {}
    }
    let base_image = image(m, BASE_IMAGE, "--executor-base-image")?;
    let kbf_image = image(m, KBF_IMAGE, "--executor-kbf-image")?;
    if base_image.digest() == kbf_image.digest() {
        return Err(format!(
            "--executor-kbf-image is the base image {base_image}: it must be an image built \
             from the base, which keeps the task tool as its entrypoint"
        ));
    }
    let mut config = Config::new(&one(FRONTEND));
    config.arch = one(ARCH);
    config.run_margin = secs(MARGIN);
    config.failed_stderr_log_bytes = usize::try_from(count(FAILED_STDERR_LOG))
        .map_err(|e| format!("--executor-failed-stderr-log-bytes: {e}"))?;
    config.max_run = secs(MAX_RUN);
    config.default_timeout = secs(DEFAULT_TIMEOUT);
    config.call_timeout = secs(CALL_TIMEOUT);
    config.poll = Duration::from_millis(count(POLL));
    config.prepare_limit = secs(PREPARE_LIMIT);
    config.input_tree = match m.get_one::<String>(INPUT_TREE).map(String::as_str) {
        Some("provider") => InputTree::Provider,
        _ => InputTree::Manifests,
    };
    config.live = secs(LIVE);
    config.mac = mac_pool(m)?;
    Ok(JsAgentFlags {
        config,
        job_url,
        token_file: m
            .get_one::<PathBuf>(TOKEN_FILE)
            .cloned()
            .unwrap_or_default(),
        ca_file,
        base_image,
        kbf_image,
        base_tree: m.get_one::<PathBuf>(BASE_TREE).cloned().unwrap_or_default(),
        base_tree_url: one(BASE_TREE_URL),
        base_tree_keep: m
            .get_one::<usize>(BASE_TREE_KEEP)
            .copied()
            .unwrap_or_default(),
        base_tree_timeout: secs(BASE_TREE_TIMEOUT),
        published: Arc::default(),
        put_retry: DEFAULT_TOTAL,
        manifest_writes: MANIFEST_WRITES,
    })
}

/// The Macs `--executor-mac-*` declare: none without `--executor-mac-task-bundle`, which the
/// other two need.
fn mac_pool(m: &ArgMatches) -> Result<Option<MacPool>, String> {
    let out_root = m.get_one::<String>(MAC_OUT_ROOT);
    let Some(bundle) = m.get_one::<String>(MAC_BUNDLE).cloned() else {
        let given = m.contains_id(MAC_OUT_ROOT)
            || m.value_source(MAC_OS_VERSION)
                .is_some_and(|source| source == clap::parser::ValueSource::CommandLine);
        return match given {
            false => Ok(None),
            true => Err(
                "--executor-mac-os-version and --executor-mac-task-out-root are for \
                 --executor-mac-task-bundle: without the bundle no Mac is used and they would \
                 do nothing"
                    .to_owned(),
            ),
        };
    };
    let roots = out_root
        .map(|root| PutDirRoots::new(root))
        .transpose()
        .map_err(|e| format!("--executor-mac-task-out-root: {e}"))?;
    Ok(Some(MacPool {
        bundle,
        os_version: m
            .get_one::<u32>(MAC_OS_VERSION)
            .copied()
            .unwrap_or(DEFAULT_MAC_OS_VERSION),
        roots,
    }))
}

/// The image the flag `long` (argument `id`) names: by digest, with or without the `docker://`
/// scheme of the `container-image` property.
fn image(m: &ArgMatches, id: &str, long: &str) -> Result<ImageRef, String> {
    let value = m.get_one::<String>(id).map_or("", String::as_str);
    let named = match value.starts_with("docker://") {
        true => value.to_owned(),
        false => format!("docker://{value}"),
    };
    ImageRef::parse(&named).map_err(|e| format!("{long}: {e}"))
}

impl JsAgentFlags {
    /// The executor's seams over the frontend's own `packs`, CAS `backend` (built over the same
    /// packs) and `source` of keys and put directories, writing each run's action tree over the
    /// base tree [`JsAgentFlags::published`] holds.
    #[must_use]
    pub fn front(&self, packs: Packs, backend: Backend, source: RunSource) -> Front {
        let put_dir = PutDirParts::new(&packs, &backend, Arc::clone(&self.published))
            .with_put_retry(PutRetry::new(self.put_retry))
            .with_write_slots(self.manifest_writes);
        Front::new(FrontParts {
            packs,
            backend,
            mint: source.mint,
            prepare: source.prepare,
            base_image: self.base_image.clone(),
            kbf_image: self.kbf_image.clone(),
            put_dir,
        })
    }

    /// The client of the job service these flags describe. Nothing is sent: the service is
    /// first contacted when the executor is stepped.
    ///
    /// # Errors
    /// The token file cannot be used (missing, readable by others, not a token), the CA file
    /// cannot be read or holds no certificate, or the URL and the CA do not go together.
    ///
    /// # Panics
    /// Outside a Tokio runtime, as [`Client::new`].
    pub fn client(&self) -> Result<Client, Box<dyn Error>> {
        let origin = Origin::from_str(&self.job_url)?;
        let token = Token::from_file(&self.token_file)
            .map_err(|e| format!("--executor-job-token-file: {e}"))?;
        let mut config = kbf_job_client::Config::new(origin, token);
        if let Some(ca) = &self.ca_file {
            let pem = std::fs::read(ca)
                .map_err(|e| format!("--executor-job-ca-file {}: {e}", ca.display()))?;
            config = config.with_ca_pem(pem);
        }
        Ok(Client::new(config).map_err(|e| format!("--executor-job-url: {e}"))?)
    }

    /// The executor these flags describe, over the seams `front`, counting into `metrics`,
    /// recording the operations' events into `recorder`, answering a finished operation by
    /// name from memory for `finished_retention`, and keeping the operations' rows in `store`
    /// (none without a state store). The job service is not contacted until it is stepped.
    ///
    /// # Errors
    /// As [`JsAgentFlags::client`], and a setting that breaks a rule ([`Config::validate`]).
    pub fn build(
        &self,
        front: Arc<Front>,
        metrics: Arc<Metrics>,
        store: Option<Arc<dyn kbf_state::Store>>,
        recorder: Recorder,
        finished_retention: Duration,
    ) -> Result<Executor, Box<dyn Error>> {
        let deps = Deps {
            client: self.client()?,
            ingest: front.clone(),
            inputs: front,
            metrics,
            store,
            recorder,
        };
        let mut config = self.config.clone();
        config.finished_retention = finished_retention;
        // The flags were checked when they were parsed.
        Ok(Executor::new(config, deps)?)
    }
}

#[cfg(test)]
#[path = "executor_flags_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "executor_flags_front_tests.rs"]
mod front_tests;
