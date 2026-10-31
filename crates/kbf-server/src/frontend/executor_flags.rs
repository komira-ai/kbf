//! The flags that choose where placed actions run: `--executor` and its companions.
//!
//! `--executor kbf-daemon`, the default, is what a frontend has always done: the farm places
//! actions on the daemons that dialled it. No executor is built, no lease server is dialled and
//! none of the companions is read. `--executor js-agent` runs them on hosts leased from the job
//! scheduler's lease server instead ([`crate::executor`]).
//!
//! The companions are the frontend's own. They are not refused with `kbf-daemon`, so a unit or
//! `kbf.toml` that names the lease server keeps its settings when the executor is switched back
//! (the rollback is the one flag). `--executor-insecure-argv-secrets` too: it is ignored with a
//! warning, since nothing reads it there, and refusing it would make the rollback a second edit.
//!
//! The executor runs on exactly one frontend. There is no flag that says a frontend is one of
//! several: any frontends that open the same state store are, so the executor itself looks for
//! another frontend's leases on the lease server and stops if it finds one
//! ([`crate::executor::Running`]).
//!
//! Every secret is a file: the lease server's token (mode 0600 or 0400) and its CA certificate.

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Arg, ArgAction, ArgMatches};
use kbf_front::{Backend, Packs};
use kbf_lease_client::{Client, Origin, Token};
use kbf_types::ImageRef;

use crate::Farm;
use crate::executor::{
    Clock, Config, Delivery, Deps, Executor, ExecutorKind, Front, FrontParts, Metrics, RunSource,
};

const KIND: &str = "executor";
const FRONTEND: &str = "executor_frontend_id";
const LEASE_URL: &str = "executor_lease_url";
const TOKEN_FILE: &str = "executor_lease_token_file";
const CA_FILE: &str = "executor_lease_ca_file";
const CLASS: &str = "executor_class";
const HOSTS: &str = "executor_hosts";
const CPUS: &str = "executor_host_cpus";
const MEMORY_GIB: &str = "executor_host_memory_gib";
const SLOTS: &str = "executor_host_slots";
const ARCH: &str = "executor_arch";
const WORK_DIR: &str = "executor_work_dir";
const PLAIN_PUT: &str = "executor_allow_plain_http_put";
const PLAIN_GRANT: &str = "executor_allow_plain_http_grant";
const INSECURE: &str = "executor_insecure_argv_secrets";
const DELIVERY: &str = "executor_secret_delivery";
const BASE_IMAGE: &str = "executor_base_image";
const KBF_IMAGE: &str = "executor_kbf_image";
const MARGIN: &str = "executor_run_margin_secs";
const TTL: &str = "executor_lease_ttl_secs";
const MAX_RUN: &str = "executor_max_run_secs";
const DEFAULT_TIMEOUT: &str = "executor_default_timeout_secs";
const CALL_TIMEOUT: &str = "executor_call_timeout_secs";
const OUTPUT_DISK: &str = "executor_output_disk_bytes";

/// The flags a `js-agent` executor cannot do without, in the order their absence is named.
const REQUIRED: [(&str, &str); 9] = [
    (LEASE_URL, "--executor-lease-url"),
    (TOKEN_FILE, "--executor-lease-token-file"),
    (FRONTEND, "--executor-frontend-id"),
    (CLASS, "--executor-class"),
    (HOSTS, "--executor-hosts"),
    (CPUS, "--executor-host-cpus"),
    (MEMORY_GIB, "--executor-host-memory-gib"),
    (BASE_IMAGE, "--executor-base-image"),
    (KBF_IMAGE, "--executor-kbf-image"),
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
                 lease server is contacted, whatever the other `--executor-*` flags say. \
                 `js-agent` runs them on hosts leased from the job scheduler's lease server, in \
                 the kbf image (`--executor-kbf-image`, built from `--executor-base-image`), \
                 whose task tool runs the action inside it; it needs the lease \
                 server's flags (`--executor-lease-url`, `--executor-lease-token-file`, and \
                 `--executor-lease-ca-file` for https), the pool's (`--executor-class`, \
                 `--executor-hosts`, `--executor-host-cpus`, `--executor-host-memory-gib`), \
                 `--executor-frontend-id`, the packs (`--pack-store-endpoint`) and the secret \
                 delivery (`--executor-secret-delivery`, with \
                 `--executor-insecure-argv-secrets` for its default `argv`). The executor runs on exactly one \
                 frontend: a second frontend that holds hosts on the same lease server stops. \
                 A restart of the frontend fails the builds in flight. Switching back to \
                 `kbf-daemon` releases the leases when the frontend is stopped, not when it is \
                 killed: a killed frontend's leases stay held until their ttl ends \
                 (`--executor-lease-ttl-secs`), unless a later `js-agent` start releases them. \
                 `--executor-insecure-argv-secrets` and `--executor-secret-delivery` are ignored \
                 with `kbf-daemon`",
            ),
        text(
            FRONTEND,
            "executor-frontend-id",
            "ID",
            "The name of this frontend on the lease server: the value of the `kbf-frontend` \
             label on its leases, by which a restarted frontend finds them and releases the \
             ones it no longer needs. 1 to 63 of letters, digits, `-`, `_` and `.`. It must be \
             different for each frontend (not copied with the config). Used only with \
             `--executor js-agent`",
        ),
        text(
            LEASE_URL,
            "executor-lease-url",
            "URL",
            "The lease server, as `https://host[:port]`, or `http://` to a loopback host. Used \
             only with `--executor js-agent`",
        ),
        path(
            TOKEN_FILE,
            "executor-lease-token-file",
            "A file with the bearer token of the lease server, readable by its owner alone (mode \
             0600 or 0400). The secret is never a flag: a command line is world-readable. Used \
             only with `--executor js-agent`",
        ),
        path(
            CA_FILE,
            "executor-lease-ca-file",
            "The CA certificate (PEM) the lease server's certificate chains to: the only trust \
             anchor, no system roots are consulted. Needed for an `https` lease server and \
             refused for an `http` one. Used only with `--executor js-agent`",
        ),
        text(
            CLASS,
            "executor-class",
            "CLASS",
            "The lease server's class every lease asks for (the pool of hosts the executor \
             draws from). Used only with `--executor js-agent`",
        ),
        Arg::new(HOSTS)
            .long("executor-hosts")
            .value_name("HOSTS")
            .value_parser(clap::value_parser!(usize))
            .help(
                "How many hosts to hold, a lease each. A host is leased whole for the shape \
                 below and released before its lease ends. Used only with `--executor js-agent`",
            ),
        Arg::new(CPUS)
            .long("executor-host-cpus")
            .value_name("CORES")
            .value_parser(clap::value_parser!(u64).range(1..))
            .help(
                "The CPU cores a host's lease holds: the pool's shape. At least 1. Used only \
                 with `--executor js-agent`",
            ),
        Arg::new(MEMORY_GIB)
            .long("executor-host-memory-gib")
            .value_name("GIB")
            .value_parser(clap::value_parser!(u64).range(1..))
            .help(
                "The memory, in GiB, a host's lease holds: the pool's shape. Each run is \
                 capped at 1.5 times its booking plus 512 MiB, and the bookings of a host add \
                 up to what is left after that. Used only with `--executor js-agent`",
            ),
        Arg::new(SLOTS)
            .long("executor-host-slots")
            .value_name("RUNS")
            .value_parser(clap::value_parser!(u64).range(1..=256))
            .default_value("8")
            .help(
                "The most runs a host takes at once, reported to the farm as the host's action \
                 slots. Used only with `--executor js-agent`",
            ),
        Arg::new(ARCH)
            .long("executor-arch")
            .value_name("ARCH")
            .value_parser(["x86_64", "arm64"])
            .default_value("x86_64")
            .help(
                "The CPU architecture of the hosts. Every lease asks for a host of `os=linux` \
                 and this `arch`, and the hosts report them to the farm. Used only with \
                 `--executor js-agent`",
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
            INSECURE,
            "executor-insecure-argv-secrets",
            "Says the secrets of a task (the presigned write url, the read token) may travel as \
             arguments of the run, where the action can read them from `/proc` and the engine's \
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
                 `argv`, the default, passes them as arguments of the run and needs \
                 `--executor-insecure-argv-secrets`. `files` passes each as a file of the run, \
                 which the lease server mounts read-only at `/run/js-files`: no argument holds \
                 a secret, `--executor-insecure-argv-secrets` is refused with it, and only hosts \
                 that report `run-files` are leased; a pool of none holds no host and no run is \
                 sent. Used only with `--executor js-agent`",
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
             that every run is launched from. It keeps the task tool as its `ENTRYPOINT`, so \
             the run's arguments are the tool's flags and the action runs inside this image. \
             Must be another image than the base. Needed with `--executor js-agent`",
        ),
        number(
            MARGIN,
            "executor-run-margin-secs",
            "What a run's wall time adds to the action's timeout, in seconds: the fetch of the \
             inputs and the upload of the pack. An action that fetches or uploads for longer \
             than this ends at the wall time and is answered to the caller as a timeout. Used \
             only with `--executor js-agent`",
            "300",
        ),
        number(
            TTL,
            "executor-lease-ttl-secs",
            "How long a lease lasts without a renewal, in seconds, asked at each renewal. It \
             must cover the longest run and two heartbeats, and the lease server grants at \
             most its own `--max-ttl`. Used only with `--executor js-agent`",
            "3600",
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
            "How long one call to the lease server may take before it counts as unanswered, in \
             seconds. Under the fence time. Used only with `--executor js-agent`",
            "5",
        ),
        Arg::new(OUTPUT_DISK)
            .long("executor-output-disk-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            .default_value("8589934592")
            .help(
                "The scratch disk a run books for what it writes, in bytes: its outputs, the \
                 pack and the private home, tmp and cache. A run books this, the bytes of its \
                 inputs and a fixed 1 GiB margin against the host's scratch, and the lease \
                 server refuses a run the host's disk cannot hold. Used only with `--executor \
                 js-agent`",
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
    /// `--executor-lease-url`.
    pub lease_url: String,
    /// `--executor-lease-token-file`.
    pub token_file: PathBuf,
    /// `--executor-lease-ca-file`.
    pub ca_file: Option<PathBuf>,
    /// `--executor-base-image`, by digest.
    pub base_image: ImageRef,
    /// `--executor-kbf-image`, by digest.
    pub kbf_image: ImageRef,
}

/// What `--executor` says: the daemons, or hosts of a lease server.
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
/// needs that is missing, a setting that breaks a rule ([`Config::validate`]), a lease server
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
            tracing::warn!(
                "--executor-secret-delivery files is ignored: it is for --executor js-agent, and this frontend runs placed actions on daemons"
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
    let files = m.get_one::<String>(DELIVERY).is_some_and(|d| d == "files");
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
    Ok(ExecutorFlags {
        kind: ExecutorKind::JsAgent,
        js: Some(js),
    })
}

fn js_agent_flags(m: &ArgMatches) -> Result<JsAgentFlags, String> {
    let one = |id: &str| m.get_one::<String>(id).cloned().unwrap_or_default();
    let count = |id: &str| m.get_one::<u64>(id).copied().unwrap_or_default();
    let secs = |id: &str| Duration::from_secs(count(id));
    let lease_url = one(LEASE_URL);
    let origin = Origin::from_str(&lease_url).map_err(|e| format!("--executor-lease-url: {e}"))?;
    let ca_file = m.get_one::<PathBuf>(CA_FILE).cloned();
    match (origin.is_tls(), ca_file.is_some()) {
        (true, false) => {
            return Err("--executor-lease-url is https: give --executor-lease-ca-file".to_owned());
        }
        (false, true) => {
            return Err("--executor-lease-ca-file is for an https --executor-lease-url".to_owned());
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
        &one(CLASS),
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
    config.insecure_argv_secrets = m.get_flag(INSECURE);
    config.delivery = delivery(m);
    config.run_margin = secs(MARGIN);
    config.lease_ttl = secs(TTL);
    config.max_run = secs(MAX_RUN);
    config.default_timeout = secs(DEFAULT_TIMEOUT);
    config.call_timeout = secs(CALL_TIMEOUT);
    config.output_disk_bytes = count(OUTPUT_DISK);
    Ok(JsAgentFlags {
        config,
        lease_url,
        token_file: m
            .get_one::<PathBuf>(TOKEN_FILE)
            .cloned()
            .unwrap_or_default(),
        ca_file,
        base_image,
        kbf_image,
    })
}

/// The delivery `--executor-secret-delivery` names.
fn delivery(m: &ArgMatches) -> Delivery {
    match m.get_one::<String>(DELIVERY).map(String::as_str) {
        Some("files") => Delivery::Files,
        _ => Delivery::InsecureArgv,
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
    /// packs) and `source` of keys, writes and grants.
    #[must_use]
    pub fn front(&self, packs: Packs, backend: Backend, source: RunSource) -> Front {
        Front::new(FrontParts {
            packs,
            backend,
            mint: source.mint,
            prepare: source.prepare,
            base_image: self.base_image.clone(),
            kbf_image: self.kbf_image.clone(),
        })
    }

    /// The client of the lease server these flags describe. Nothing is sent: the server is
    /// first contacted when the executor is stepped.
    ///
    /// # Errors
    /// The token file cannot be used (missing, readable by others, not a token), the CA file
    /// cannot be read or holds no certificate, or the URL and the CA do not go together.
    pub fn client(&self) -> Result<Client, Box<dyn Error>> {
        let origin = Origin::from_str(&self.lease_url)?;
        let token = Token::from_file(&self.token_file)
            .map_err(|e| format!("--executor-lease-token-file: {e}"))?;
        let mut lease = kbf_lease_client::Config::new(origin, token);
        if let Some(ca) = &self.ca_file {
            let pem = std::fs::read(ca)
                .map_err(|e| format!("--executor-lease-ca-file {}: {e}", ca.display()))?;
            lease = lease.with_ca_pem(pem);
        }
        Ok(Client::new(lease).map_err(|e| format!("--executor-lease-url: {e}"))?)
    }

    /// The executor these flags describe, over `farm` and the seams `front`, counting into
    /// `metrics`. The lease server is not contacted until it is stepped.
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        let clock = Clock::from_unix_ms(u64::try_from(now).unwrap_or(u64::MAX));
        // The flags were checked when they were parsed.
        Ok(Executor::new(self.config.clone(), deps, clock)?)
    }
}

#[cfg(test)]
#[path = "executor_flags_tests.rs"]
mod tests;
