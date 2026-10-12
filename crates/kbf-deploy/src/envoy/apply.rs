//! `envoy apply`: put a client front's objects on its cluster with `kubectl`, after
//! checking that what they need is there. Without `--apply`, the API server only
//! validates them (a server-side dry run) and nothing is written.
//!
//! In order, each refusing (exit 2) before anything is applied:
//!
//! 1. The settings are not the example's: `KBF_FRONT_KUBE_CONTEXT`,
//!    `KBF_FRONT_TAILNET_DNS` and `KBF_SERVERS` must each differ from the example
//!    config's (`--refuse-example FILE`, or kbf's `templates/envoy/config.example`).
//! 2. The render of the settings passes every `envoy check` check; each failure is
//!    printed as its `FAIL <check>: <why>` line.
//! 3. What the cluster's admin creates by hand, read only: the `tailscale`
//!    IngressClass (the Tailscale operator is installed), the operator's OAuth
//!    Secret, the front's namespace, and the CA Secret with a `ca.pem` key. No
//!    Secret's data is printed: for the CA, kubectl prints `yes` when the key is there.
//! 4. `kubectl diff` of the render, printed: what `--apply` would change. When it
//!    finds no difference the cluster already holds the render, and the plan is empty.
//! 5. `kubectl apply --dry-run=server` of the render: the API server's own validation.
//!
//! Steps 3 to 5 run in a dry run as in an apply and change nothing. When the diff
//! found a change, the plan is the apply and then `kubectl rollout status` of the Envoy
//! Deployment (at most 180 s), printed as `DRY-RUN:` lines by default and carried out
//! only with `--apply`, as a user other than root. After an apply (or an `--apply`
//! with nothing to change), the Ingress must report the address
//! `<KBF_FRONT_HOSTNAME>.<KBF_FRONT_TAILNET_DNS>` within 120 s, asked every 2 s;
//! another address or none fails (exit 1), as a failed step does.
//!
//! Every diff and apply is server-side under the field manager `kbf-front`, so a field
//! another manager owns stops the apply with a conflict instead of being taken over.
//! It never forces, prunes or deletes, and creates no Secret or namespace.
//!
//! `kubectl` runs by absolute path with an empty environment, and gets
//! `--kubeconfig`, a private `--cache-dir` and the config's `--context` on every call,
//! so the kubeconfig's current context is never used. A kubeconfig whose user runs an
//! exec credential plugin is not supported.

use std::io::{self, Write};
use std::path::Path;
use std::process::{ExitStatus, Output};
use std::time::Duration;

use super::check::{self, CheckFailure};
use super::config::{EnvoyConfig, EnvoyError};
use super::render::{APP, INGRESS, render};
use kbf_host::exec::Exec;
use kbf_host::path::HostPath;
use kbf_host::plan::{Mode, Plan, Planner, RunAs, execute};
use kbf_host::step::Cmd;

/// kbf's example settings, which `apply` refuses when no other example is given.
pub const EXAMPLE: &str = include_str!("../../templates/envoy/config.example");
/// The server-side apply field manager of every object the front applies.
pub const FIELD_MANAGER: &str = "kbf-front";
/// The IngressClass the Tailscale operator installs.
pub const INGRESS_CLASS: &str = "tailscale";
/// How long `kubectl rollout status` waits for Envoy.
pub const ROLLOUT_TIMEOUT: &str = "180s";

/// How often, and how many times, the Ingress's address is asked for after an apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wait {
    pub tries: u32,
    pub every: Duration,
}

impl Wait {
    /// 60 times, 2 s apart.
    pub const DEFAULT: Wait = Wait {
        tries: 60,
        every: Duration::from_secs(2),
    };
}

/// What `envoy apply` is asked to do.
pub struct Apply {
    pub config: EnvoyConfig,
    /// The settings `config` must not share a context, tailnet or server list with.
    pub example: EnvoyConfig,
    pub kubectl: HostPath,
    pub kubeconfig: HostPath,
    /// kubectl's `--cache-dir`: a private scratch directory, removed after the run.
    pub cache_dir: String,
    pub wait: Wait,
}

impl Apply {
    fn kubectl(&self, args: &[&str]) -> Cmd {
        let mut cmd = Cmd::new(self.kubectl.clone())
            .arg("--kubeconfig")
            .arg(self.kubeconfig.as_str())
            .arg("--cache-dir")
            .arg(&self.cache_dir)
            .arg("--context")
            .arg(&self.config.kube_context);
        for arg in args {
            cmd = cmd.arg(*arg);
        }
        cmd
    }
}

/// Why `envoy apply` refused (exit 2) or failed (exit 1, [`ApplyError::exit_code`]).
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Config(#[from] EnvoyError),
    #[error("{0} is still the example config's value: fill it in")]
    Example(&'static str),
    #[error("the render fails {} check(s), each on a FAIL line; nothing was applied", .0.len())]
    Check(Vec<CheckFailure>),
    #[error("the render is not JSON: {0}")]
    Render(serde_json::Error),
    #[error("{what} ({hint}); nothing was applied. kubectl: {stderr}")]
    Missing {
        what: String,
        hint: &'static str,
        stderr: String,
    },
    #[error(
        "Secret {0} has no key ca.pem (the CA the servers' certificates chain to); nothing was applied"
    )]
    NoCaKey(String),
    #[error("{cmd}: {source}")]
    Query { cmd: String, source: io::Error },
    #[error("kubectl diff failed ({status}); nothing was applied: {stderr}")]
    Diff { status: ExitStatus, stderr: String },
    #[error("the server-side dry run refused the objects; nothing was applied: {0}")]
    DryRun(String),
    #[error(
        "the Ingress {ns}/{INGRESS} has no address after {secs} s (kubectl -n {ns} describe \
         ingress {INGRESS}, and the operator's logs)"
    )]
    NoAddress { ns: String, secs: u64 },
    #[error(
        "the Ingress's address is {got}, the config expects {want} (another device holds \
         the name, or KBF_FRONT_TAILNET_DNS is wrong)"
    )]
    WrongAddress { got: String, want: String },
    #[error("cannot make a scratch directory: {0}")]
    Scratch(io::Error),
    #[error("cannot write the plan out: {0}")]
    Output(io::Error),
    #[error(transparent)]
    Host(#[from] kbf_host::HostError),
}

impl ApplyError {
    /// 1 when a step or the check after an apply failed, 2 when the apply was refused
    /// before any step.
    pub fn exit_code(&self) -> u8 {
        match self {
            ApplyError::Host(kbf_host::HostError::Step { .. })
            | ApplyError::NoAddress { .. }
            | ApplyError::WrongAddress { .. } => 1,
            _ => 2,
        }
    }
}

/// The example settings: the file at `path`, or kbf's [`EXAMPLE`].
pub fn example(path: Option<&Path>) -> Result<EnvoyConfig, EnvoyError> {
    match path {
        Some(path) => EnvoyConfig::load(path),
        None => EnvoyConfig::parse(EXAMPLE),
    }
}

/// `envoy apply`: the checks, then the plan printed or (in [`Mode::Apply`]) carried out
/// through `exec`, then the Ingress's address. What the checks found goes to `out`.
pub fn run<E: Exec, W: Write>(
    exec: &mut E,
    args: &Apply,
    mode: Mode,
    out: &mut W,
) -> Result<(), ApplyError> {
    refuse_example(&args.config, &args.example)?;
    let manifests = render(&args.config).manifests;
    check_render(&manifests, &args.config.operator_namespace, out)?;
    prerequisites(&*exec, args, out)?;
    let plan = plan(&*exec, args, &manifests, out)?;
    execute(&plan, RunAs::User, mode, exec, out)?;
    if mode == Mode::Apply {
        let want = address(&*exec, args)?;
        note(
            out,
            &format!("the front is up: clients dial grpcs://{want}:443"),
        )?;
    }
    Ok(())
}

/// Refuses `config` when it still has `example`'s context, tailnet or servers.
fn refuse_example(config: &EnvoyConfig, example: &EnvoyConfig) -> Result<(), ApplyError> {
    let same = [
        (
            "KBF_FRONT_KUBE_CONTEXT",
            config.kube_context == example.kube_context,
        ),
        (
            "KBF_FRONT_TAILNET_DNS",
            config.tailnet_dns == example.tailnet_dns,
        ),
        ("KBF_SERVERS", config.servers == example.servers),
    ];
    match same.into_iter().find(|(_, same)| *same) {
        Some((key, _)) => Err(ApplyError::Example(key)),
        None => Ok(()),
    }
}

/// Runs every `envoy check` check on `manifests`; each failure is printed to `out`.
fn check_render<W: Write>(
    manifests: &str,
    operator_namespace: &str,
    out: &mut W,
) -> Result<(), ApplyError> {
    let doc = serde_json::from_str(manifests).map_err(ApplyError::Render)?;
    let failures = check::check(&doc, operator_namespace);
    for f in &failures {
        note(out, &f.line())?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ApplyError::Check(failures))
    }
}

/// What the cluster's admin creates before the first apply; each is read, never
/// changed.
fn prerequisites<E: Exec, W: Write>(exec: &E, args: &Apply, out: &mut W) -> Result<(), ApplyError> {
    let c = &args.config;
    let (ns, opns) = (c.namespace.as_str(), c.operator_namespace.as_str());
    let present = [
        (
            args.kubectl(&["get", "ingressclass", INGRESS_CLASS, "-o", "name"]),
            format!("IngressClass {INGRESS_CLASS}"),
            "the Tailscale operator is not installed",
        ),
        (
            args.kubectl(&[
                "-n",
                opns,
                "get",
                "secret",
                &c.operator_secret,
                "-o",
                "name",
            ]),
            format!("Secret {opns}/{}", c.operator_secret),
            "the operator's OAuth client",
        ),
        (
            args.kubectl(&["get", "namespace", ns, "-o", "name"]),
            format!("namespace {ns}"),
            "the front's namespace",
        ),
    ];
    for (cmd, what, hint) in present {
        let answer = query(exec, &cmd)?;
        if !answer.status.success() {
            return Err(missing(what, hint, &answer));
        }
        note(out, &format!("{what}: present"))?;
    }
    let ca = format!("{ns}/{}", c.ca_secret);
    let has_key = r#"go-template={{if index .data "ca.pem"}}yes{{end}}"#;
    let cmd = args.kubectl(&["-n", ns, "get", "secret", &c.ca_secret, "-o", has_key]);
    let answer = query(exec, &cmd)?;
    if !answer.status.success() {
        return Err(missing(format!("Secret {ca}"), "the servers' CA", &answer));
    }
    if String::from_utf8_lossy(&answer.stdout).trim() != "yes" {
        return Err(ApplyError::NoCaKey(ca));
    }
    note(out, &format!("Secret {ca}: present, with ca.pem"))
}

fn missing(what: String, hint: &'static str, answer: &Output) -> ApplyError {
    ApplyError::Missing {
        what: format!("no {what}"),
        hint,
        stderr: String::from_utf8_lossy(&answer.stderr)
            .trim_end()
            .to_owned(),
    }
}

/// The diff and the server-side dry run, then the plan: the apply and Envoy's rollout
/// when the diff found a change, nothing when it found none.
fn plan<E: Exec, W: Write>(
    exec: &E,
    args: &Apply,
    manifests: &str,
    out: &mut W,
) -> Result<Plan, ApplyError> {
    let manager = format!("--field-manager={FIELD_MANAGER}");
    let bytes = manifests.as_bytes().to_vec();
    let diff = args
        .kubectl(&["diff", "--server-side", &manager, "-f", "-"])
        .stdin(bytes.clone());
    let answer = query(exec, &diff)?;
    // kubectl diff exits 0 for no difference, 1 for a difference, more for an error.
    let differs = match answer.status.code() {
        Some(0) => false,
        Some(1) => true,
        _ => {
            return Err(ApplyError::Diff {
                status: answer.status,
                stderr: String::from_utf8_lossy(&answer.stderr)
                    .trim_end()
                    .to_owned(),
            });
        }
    };
    if differs {
        note(out, "kubectl diff, what --apply changes:")?;
        out.write_all(&answer.stdout).map_err(ApplyError::Output)?;
    } else {
        note(out, "kubectl diff: the cluster holds the render")?;
    }
    let dry_run = args
        .kubectl(&[
            "apply",
            "--server-side",
            &manager,
            "--dry-run=server",
            "-f",
            "-",
        ])
        .stdin(bytes.clone());
    let answer = query(exec, &dry_run)?;
    if !answer.status.success() {
        let stderr = String::from_utf8_lossy(&answer.stderr);
        return Err(ApplyError::DryRun(stderr.trim_end().to_owned()));
    }
    note(out, "kubectl apply --dry-run=server: accepted")?;
    let mut planner = Planner::new(exec);
    if differs {
        let ns = args.config.namespace.as_str();
        let deployment = format!("deployment/{APP}");
        let timeout = format!("--timeout={ROLLOUT_TIMEOUT}");
        planner.run(
            args.kubectl(&["apply", "--server-side", &manager, "-f", "-"])
                .stdin(bytes),
        );
        planner.run(args.kubectl(&["-n", ns, "rollout", "status", &deployment, &timeout]));
    }
    Ok(planner.finish())
}

/// The Ingress's address once the operator has given it one, which must be the
/// config's `<hostname>.<tailnet>`; a trailing `.` is dropped.
fn address<E: Exec>(exec: &E, args: &Apply) -> Result<String, ApplyError> {
    let c = &args.config;
    let want = format!("{}.{}", c.hostname, c.tailnet_dns);
    let jsonpath = "jsonpath={.status.loadBalancer.ingress[0].hostname}";
    let cmd = args.kubectl(&[
        "-n",
        &c.namespace,
        "get",
        "ingress",
        INGRESS,
        "-o",
        jsonpath,
    ]);
    for attempt in 0..args.wait.tries {
        if attempt > 0 {
            std::thread::sleep(args.wait.every);
        }
        let answer = query(exec, &cmd)?;
        // An Ingress the API server cannot show yet has no address yet.
        let text = String::from_utf8_lossy(&answer.stdout);
        let got = text.trim().trim_end_matches('.');
        if !answer.status.success() || got.is_empty() {
            continue;
        }
        if got == want {
            return Ok(want);
        }
        return Err(ApplyError::WrongAddress {
            got: got.to_owned(),
            want,
        });
    }
    Err(ApplyError::NoAddress {
        ns: c.namespace.clone(),
        secs: args.wait.every.as_secs() * u64::from(args.wait.tries),
    })
}

fn query<E: Exec>(exec: &E, cmd: &Cmd) -> Result<Output, ApplyError> {
    exec.query(cmd).map_err(|source| ApplyError::Query {
        cmd: cmd.to_string(),
        source,
    })
}

fn note<W: Write>(out: &mut W, line: &str) -> Result<(), ApplyError> {
    writeln!(out, "{line}").map_err(ApplyError::Output)
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
