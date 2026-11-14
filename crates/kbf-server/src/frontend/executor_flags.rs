//! The flags that choose where placed actions run: `--executor` and its companions.
//!
//! `--executor kbf-daemon`, the default, is what a frontend has always done: the farm places
//! actions on the daemons that dialled it. No executor is built, no job service is dialled and
//! none of the companions is read. `--executor js-agent` runs each of them as a job of the job
//! scheduler's job service instead ([`crate::executor`]).
//!
//! The companions are the frontend's own. They are not refused with `kbf-daemon`, so a unit or
//! `kbf.toml` that names the job service keeps its settings when the executor is switched back
//! (the rollback is the one flag). `--executor-insecure-argv-secrets` too: it is ignored with a
//! warning, since nothing reads it there, and refusing it would make the rollback a second edit.
//!
//! The executor runs on exactly one frontend. There is no flag that says a frontend is one of
//! several: any frontends that open the same state store are, so the executor itself looks for
//! another frontend's jobs on the job service and stops if it finds one
//! ([`crate::executor::Running`]).
//!
//! The executor speaks to a job service only. The flags of the lease server it once leased
//! hosts from (`--executor-lease-url`, `--executor-lease-token-file`,
//! `--executor-lease-ca-file`, `--executor-lease-ttl-secs`, `--executor-class`) are gone, not
//! aliased: a frontend started with one fails at start, naming it, and never runs on the wrong
//! protocol.
//!
//! A job books no disk: the pool books none, and a job writes to its host's scratch with no
//! quota. `--executor-output-disk-bytes`, the scratch a job once asked for its outputs, is gone
//! the same way: a frontend started with it, with either executor, fails at start, naming it.
//!
//! Every secret is a file: the job service's token (mode 0600 or 0400) and its CA certificate.

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches};
use kbf_front::action_tree::BaseTree;
use kbf_front::{Backend, Packs};
use kbf_job_client::{Client, Origin, Token};
use kbf_types::ImageRef;

use crate::Farm;
use crate::executor::{
    Config, Delivery, Deps, Executor, ExecutorKind, Front, FrontParts, Metrics, Network,
    PutDirParts, RunSource, TaskOut,
};

const KIND: &str = "executor";
const FRONTEND: &str = "executor_frontend_id";
const JOB_URL: &str = "executor_job_url";
const TOKEN_FILE: &str = "executor_job_token_file";
const CA_FILE: &str = "executor_job_ca_file";
const HOSTS: &str = "executor_hosts";
const CPUS: &str = "executor_host_cpus";
const MEMORY_GIB: &str = "executor_host_memory_gib";
const SLOTS: &str = "executor_host_slots";
const ARCH: &str = "executor_arch";
const WORK_DIR: &str = "executor_work_dir";
const PLAIN_PUT: &str = "executor_allow_plain_http_put";
const PLAIN_GRANT: &str = "executor_allow_plain_http_grant";
const LOCATE: &str = "executor_locate";
const INSECURE: &str = "executor_insecure_argv_secrets";
const DELIVERY: &str = "executor_secret_delivery";
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
const MAX_RUN: &str = "executor_max_run_secs";
const DEFAULT_TIMEOUT: &str = "executor_default_timeout_secs";
const CALL_TIMEOUT: &str = "executor_call_timeout_secs";
const NETWORK: &str = "executor_network";
const OPEN_NETWORK: &str = "executor_allow_open_network";

/// The flags a `js-agent` executor cannot do without, in the order their absence is named.
const REQUIRED: [(&str, &str); 9] = [
    (JOB_URL, "--executor-job-url"),
    (TOKEN_FILE, "--executor-job-token-file"),
    (FRONTEND, "--executor-frontend-id"),
    (HOSTS, "--executor-hosts"),
    (CPUS, "--executor-host-cpus"),
    (MEMORY_GIB, "--executor-host-memory-gib"),
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

fn switch(id: &'static str, long: &'static str, help: &'static str) -> Arg {
    Arg::new(id)
        .long(long)
        .action(ArgAction::SetTrue)
        .help(help)
}

/// The flags.
pub(super) fn args() -> Vec<Arg> {
    vec![
        Arg::new(KIND)
            .long("executor")
            .value_name("KIND")
            .value_parser(["kbf-daemon", "js-agent"])
            .default_value("kbf-daemon")
            .help(
                "Where the farm runs the actions it places. `kbf-daemon`, the default, is the \
                 daemon of each node, over the worker listener: no executor is built and no \
                 job service is contacted, whatever the other `--executor-*` flags say. \
                 `js-agent` runs each as a job of the job scheduler's job service, in the kbf \
                 image (`--executor-kbf-image`, built from `--executor-base-image`), whose \
                 task tool runs the action inside it; it needs the job service's flags \
                 (`--executor-job-url`, `--executor-job-token-file`, and \
                 `--executor-job-ca-file` for https), the virtual workers' \
                 (`--executor-hosts`, `--executor-host-cpus`, `--executor-host-memory-gib`), \
                 `--executor-frontend-id`, the release's base tree bundles \
                 (`--executor-base-tree`), the packs (`--pack-store-endpoint`) and the secret \
                 delivery (`--executor-secret-delivery`, with \
                 `--executor-insecure-argv-secrets` for its default `argv`). The executor runs \
                 on exactly one frontend: a second frontend that has jobs on the same job \
                 service stops. A restart of the frontend fails the builds in flight. \
                 Switching back to `kbf-daemon` cancels the jobs when the frontend is stopped, \
                 not when it is killed: a killed frontend's jobs run to their wall limit, \
                 unless a later `js-agent` start cancels them. \
                 `--executor-insecure-argv-secrets` is ignored with `kbf-daemon`",
            ),
        text(
            FRONTEND,
            "executor-frontend-id",
            "ID",
            "The name of this frontend on the job service: the value of the `kbf-frontend` \
             label on its jobs, by which a restarted frontend finds and cancels the ones an \
             earlier process left, hashed into every job id, and the name of its virtual \
             workers (`js:<ID>-<n>`). 1 to 63 of letters, digits, `-`, `_` and `.`. It must be \
             different for each frontend (not copied with the config). Used only with \
             `--executor js-agent`",
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
        Arg::new(HOSTS)
            .long("executor-hosts")
            .value_name("WORKERS")
            .value_parser(clap::value_parser!(usize))
            .help(
                "How many virtual workers the farm places actions on, each of the shape below: \
                 how many actions of which size the farm may have in flight as jobs. The job \
                 service decides which host runs each job. Used only with `--executor js-agent`",
            ),
        Arg::new(CPUS)
            .long("executor-host-cpus")
            .value_name("CORES")
            .value_parser(clap::value_parser!(u64).range(1..))
            .help(
                "The CPU cores of a virtual worker: the most the farm books on it at once. At \
                 least 1. Used only with `--executor js-agent`",
            ),
        Arg::new(MEMORY_GIB)
            .long("executor-host-memory-gib")
            .value_name("GIB")
            .value_parser(clap::value_parser!(u64).range(1..))
            .help(
                "The memory, in GiB, of a virtual worker. Each job needs 1.5 times its booking \
                 plus 512 MiB, and the bookings of a worker add up to what is left after that. \
                 Used only with `--executor js-agent`",
            ),
        Arg::new(SLOTS)
            .long("executor-host-slots")
            .value_name("RUNS")
            .value_parser(clap::value_parser!(u64).range(1..=256))
            .default_value("8")
            .help(
                "The most actions a virtual worker has in flight at once, reported to the farm \
                 as its action slots. Used only with `--executor js-agent`",
            ),
        Arg::new(ARCH)
            .long("executor-arch")
            .value_name("ARCH")
            .value_parser(["x86_64", "arm64"])
            .default_value("x86_64")
            .help(
                "The CPU architecture of the hosts. Every job asks for a host of `os=linux`, \
                 this `arch` and containers, and the virtual workers report them to the farm. \
                 Used only with `--executor js-agent`",
            ),
        text(
            WORK_DIR,
            "executor-work-dir",
            "DIR",
            "Where the task tool works in the container, if not in its own default: the kbf \
             image must have a writable directory there. Used only with `--executor js-agent`",
        ),
        switch(
            PLAIN_PUT,
            "executor-allow-plain-http-put",
            "Lets the task write its pack to an `http://` url, until the store serves TLS. The \
             reads stay https unless `--executor-allow-plain-http-grant` says otherwise. Used \
             only with `--executor js-agent`",
        ),
        switch(
            PLAIN_GRANT,
            "executor-allow-plain-http-grant",
            "Lets the task read its inputs through a read grant served over `http://`: an \
             `http://` `--task-read-url` is accepted, and the task tool is given \
             `--allow-plain-http-grant`. The read token then crosses the job network in clear, so \
             this is for a farm on its own network. Off by default, and an `http://` read url is \
             refused without it. Used only with `--executor js-agent`",
        ),
        switch(
            LOCATE,
            "executor-locate",
            "Has the task tool ask the read listener where each input is kept (`GET /locate`). \
             The answer names no store and no grant (tierstore v0.3.29 removed the store read \
             grants), so the task still reads every input through the listener, as without the \
             flag; every byte is checked against its digest. Off by default. Used only with \
             `--executor js-agent`, and refused with `--executor kbf-daemon`",
        ),
        switch(
            INSECURE,
            "executor-insecure-argv-secrets",
            "Says the secrets of a task (the presigned write url, the read token) may travel as \
             arguments of the job, where the action can read them from `/proc` and the engine's \
             inspect shows them. It is what the default `--executor-secret-delivery argv` needs, \
             and `--executor-secret-delivery files` refuses it. Off by default",
        ),
        Arg::new(DELIVERY)
            .long("executor-secret-delivery")
            .value_name("MODE")
            .value_parser(["argv", "files"])
            .default_value("argv")
            .help(
                "How the secrets of a task (the presigned write url, the read token) reach it. \
                 `argv`, the default, passes them as arguments of the job and needs \
                 `--executor-insecure-argv-secrets`. `files` passes each as a file of the job, \
                 which its host writes read-only at `/run/js-files`: no argument holds a \
                 secret, and `--executor-insecure-argv-secrets` is refused with it; a host whose \
                 agent cannot write files refuses to start the job, and the farm runs the \
                 action again. `files` is refused with `--executor kbf-daemon`, which builds no \
                 command line",
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
                 start, naming this flag. Needed with `--executor js-agent`; with \
                 `--executor kbf-daemon` nothing is read, fetched or written",
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
             seconds. Under the fence time. Used only with `--executor js-agent`",
            "5",
        ),
        Arg::new(NETWORK)
            .long("executor-network")
            .value_name("LEVEL")
            .value_parser(["none", "allow-list", "open"])
            .default_value("allow-list")
            .help(
                "The network every job is given on its host. `allow-list`, the default, is the \
                 endpoints the host's agent is set up to allow and nothing else: the task \
                 reaches the read listener and the pack store only once each agent allows them, \
                 and a host whose agent cannot check those limits refuses the job \
                 (`egress-unchecked`), which the farm runs again. `none` is no route out: every \
                 action fails, as the task can reach neither. `open` is any address but those \
                 the agent always denies, and needs `--executor-allow-open-network`. Used only \
                 with `--executor js-agent`",
            ),
        switch(
            OPEN_NETWORK,
            "executor-allow-open-network",
            "Says jobs may be given an open network: what `--executor-network open` needs, and \
             refused with any other `--executor-network`. Off by default. Used only with \
             `--executor js-agent`",
        ),
    ]
}

/// Whether the flags say a read grant may be served over plain http: `--executor js-agent` with
/// `--executor-allow-plain-http-grant`. With daemons nothing reads the flag, so it opens nothing.
pub(super) fn plain_http_grant(m: &ArgMatches) -> bool {
    m.get_one::<String>(KIND).is_some_and(|k| k == "js-agent") && m.get_flag(PLAIN_GRANT)
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
}

/// What `--executor` says: the daemons, or jobs of a job service.
#[derive(Clone, Debug)]
pub struct ExecutorFlags {
    /// Which executor.
    pub kind: ExecutorKind,
    /// The settings of `js-agent`; `None` for `kbf-daemon`.
    pub js: Option<JsAgentFlags>,
}

impl Default for ExecutorFlags {
    fn default() -> Self {
        Self {
            kind: ExecutorKind::KbfDaemon,
            js: None,
        }
    }
}

/// The setting the flags in `m` make.
///
/// # Errors
/// For `js-agent`, a flag it
/// needs that is missing, a setting that breaks a rule ([`Config::validate`]), a job service
/// URL that is not one or does not agree with the CA flag, or a base or kbf image that is not
/// pinned by digest or is the same image twice.
pub(super) fn from_matches(m: &ArgMatches) -> Result<ExecutorFlags, String> {
    let js_agent = m.get_one::<String>(KIND).is_some_and(|k| k == "js-agent");
    if !js_agent {
        // Every setting of the executor may stay in the unit or `kbf.toml` after the rollback,
        // this one too: nothing reads it here, and refusing it would make the rollback a second
        // edit, in an incident.
        if m.get_flag(INSECURE) {
            tracing::warn!(
                "--executor-insecure-argv-secrets is ignored: it is for --executor js-agent, and this frontend runs placed actions on daemons"
            );
        }
        if delivery(m) == Delivery::Files {
            return Err(
                "--executor-secret-delivery files is for --executor js-agent: daemons build no \
                 task command line, so with --executor kbf-daemon it would do nothing"
                    .to_owned(),
            );
        }
        if m.get_flag(LOCATE) {
            return Err(
                "--executor-locate is for --executor js-agent: daemons build no task command \
                 line, so with --executor kbf-daemon it would do nothing"
                    .to_owned(),
            );
        }
        return Ok(ExecutorFlags::default());
    }
    let missing: Vec<&str> = REQUIRED
        .iter()
        .filter(|(id, _)| !m.contains_id(id))
        .map(|(_, long)| *long)
        .collect();
    if !missing.is_empty() {
        return Err(format!("--executor js-agent needs {}", missing.join(", ")));
    }
    let files = delivery(m) == Delivery::Files;
    if files && m.get_flag(INSECURE) {
        return Err(
            "--executor-secret-delivery files sends no secret in arguments: drop \
             --executor-insecure-argv-secrets"
                .to_owned(),
        );
    }
    if !files && !m.get_flag(INSECURE) {
        return Err(
            "--executor js-agent needs --executor-insecure-argv-secrets, which \
                    lets the secrets of a task travel as arguments where the action can read \
                    them, or --executor-secret-delivery files"
                .to_owned(),
        );
    }
    let js = js_agent_flags(m)?;
    js.config
        .validate()
        .map_err(|e| format!("--executor js-agent: {e}"))?;
    // The network every action's command will have, logged at start: an open one, which the
    // operator asked for twice, as a warning.
    match js.config.network {
        Network::Open => tracing::warn!(
            "--executor-network open with --executor-allow-open-network: every job may reach \
             any address but those its host's agent always denies"
        ),
        network => tracing::info!(?network, "--executor-network: the network of every job"),
    }
    Ok(ExecutorFlags {
        kind: ExecutorKind::JsAgent,
        js: Some(js),
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
    let hosts = m.get_one::<usize>(HOSTS).copied().unwrap_or_default();
    let mut config = Config::new(
        &one(FRONTEND),
        hosts,
        count(CPUS).saturating_mul(1_000),
        count(MEMORY_GIB).saturating_mul(1 << 30),
    );
    config.slots = count(SLOTS);
    config.arch = one(ARCH);
    config.work_dir = m.get_one::<String>(WORK_DIR).cloned();
    if config
        .work_dir
        .as_deref()
        .is_some_and(|d| !d.starts_with('/'))
    {
        return Err("--executor-work-dir is not an absolute path".to_owned());
    }
    config.allow_plain_http_put = m.get_flag(PLAIN_PUT);
    config.allow_plain_http_grant = m.get_flag(PLAIN_GRANT);
    config.locate = m.get_flag(LOCATE);
    config.insecure_argv_secrets = m.get_flag(INSECURE);
    config.delivery = delivery(m);
    config.run_margin = secs(MARGIN);
    config.max_run = secs(MAX_RUN);
    config.default_timeout = secs(DEFAULT_TIMEOUT);
    config.call_timeout = secs(CALL_TIMEOUT);
    config.network = network(m);
    config.allow_open_network = m.get_flag(OPEN_NETWORK);
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
    })
}

/// The delivery `--executor-secret-delivery` names.
fn delivery(m: &ArgMatches) -> Delivery {
    match m.get_one::<String>(DELIVERY).map(String::as_str) {
        Some("files") => Delivery::Files,
        _ => Delivery::InsecureArgv,
    }
}

/// The network `--executor-network` names.
fn network(m: &ArgMatches) -> Network {
    match m.get_one::<String>(NETWORK).map(String::as_str) {
        Some("none") => Network::None,
        Some("open") => Network::Open,
        _ => Network::AllowList,
    }
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
    /// packs) and `source` of keys, writes and grants; with `--task-out-mode put-dir`, writing
    /// each run's action tree over the base tree [`JsAgentFlags::published`] holds.
    #[must_use]
    pub fn front(&self, packs: Packs, backend: Backend, source: RunSource) -> Front {
        let put_dir = match self.config.task_out {
            TaskOut::PutDir(_) => Some(PutDirParts::new(
                &packs,
                &backend,
                Arc::clone(&self.published),
            )),
            TaskOut::Presigned => None,
        };
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

    /// The executor these flags describe, over `farm` and the seams `front`, counting into
    /// `metrics`. The job service is not contacted until it is stepped.
    ///
    /// # Errors
    /// As [`JsAgentFlags::client`].
    pub fn build(
        &self,
        farm: Arc<Farm>,
        front: Arc<Front>,
        metrics: Arc<Metrics>,
    ) -> Result<Executor, Box<dyn Error>> {
        let deps = Deps {
            farm,
            client: self.client()?,
            ingest: front.clone(),
            presign: front.clone(),
            inputs: front,
            metrics,
        };
        // The flags were checked when they were parsed.
        Ok(Executor::new(self.config.clone(), deps)?)
    }
}

#[cfg(test)]
#[path = "executor_flags_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "executor_flags_front_tests.rs"]
mod front_tests;
