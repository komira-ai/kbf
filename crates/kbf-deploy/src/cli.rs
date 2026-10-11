//! The `kbf-deploy` command line.
//!
//! ```text
//! kbf-deploy minio buckets --endpoint URL --bucket NAME... --policy NAME --account NAME
//!                          --mc PATH [--key-file PATH [--key-owner USER]] [--remove] [--apply]
//! kbf-deploy minio nodeport --kubectl PATH --kubeconfig PATH [--context NAME]
//!                           --namespace NS --service NAME --node-port N [--remove] [--apply]
//! kbf-deploy linux render-units --config FILE --out DIR [--kbf-uid UID]
//! kbf-deploy linux check-units --config FILE --dir DIR [--installable]
//! kbf-deploy linux install-units --config FILE [--bin-dir DIR] [--restart-minio] [--apply]
//! kbf-deploy linux install-units --config FILE --uninstall [--apply]
//! kbf-deploy linux image-pull --config FILE [--image REF]... [--platform OS/ARCH] [--apply]
//! kbf-deploy linux swap --config FILE [--kubelet-allows-swap] [--remove] [--apply]
//! ```
//!
//! Bad flags exit 2 for every subcommand. The `minio` subcommands change a host: without
//! `--apply` they print their plan as `DRY-RUN:` lines and change nothing, and they exit
//! 0 done (or nothing to do) or 1 refused or failed. `linux render-units` and
//! `check-units` only write into or read the directory they are given, and exit 0 done,
//! 1 a check found a broken rule, or 2 refused (a bad setting or file, printed as
//! `REFUSED: ...` on stderr). `linux install-units`, `image-pull` and `swap` change the
//! host as root, plan by default as the `minio` subcommands do, and exit 0 done (or
//! nothing to do), 1 a step or the check after an apply failed (`FAIL ...`), or 2
//! refused (`REFUSED: ...` on stderr).
//! `minio buckets` reads the MinIO admin key pair from `MINIO_ROOT_USER` and
//! `MINIO_ROOT_PASSWORD` in the environment (secrets are never flags: argv is
//! world-readable).

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use kbf_types::ImageRef;

use crate::linux::{self, HostError, LinuxConfig, images, install, swap};
use crate::minio::{self, Admin, K8sName, KbfName, MinioError, buckets, nodeport};
use kbf_host::exec::{Exec, HostExec};
use kbf_host::path::{HostPath, Root};
use kbf_host::plan::{Mode, RunAs, execute};
use kbf_host::step::HttpUrl;

#[derive(Parser)]
#[command(
    name = "kbf-deploy",
    version,
    about = "Deploy kits for a kbf farm; a minio plan is printed by default and carried out with --apply"
)]
struct Cli {
    #[command(subcommand)]
    kit: Kit,
}

#[derive(Subcommand)]
enum Kit {
    /// A MinIO for kbf: its buckets and key, and a NodePort for its S3 port.
    #[command(subcommand)]
    Minio(Minio),
    /// A converged Linux storage node: MinIO, kbf-server and kbf-daemon.
    #[command(subcommand)]
    Linux(Linux),
}

#[derive(Subcommand)]
enum Linux {
    /// Render the node's four systemd files into a directory.
    RenderUnits {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Where to write the files, laid out as under /etc/systemd/system.
        #[arg(long)]
        out: PathBuf,
        /// The kbf user's uid (the daemon's XDG_RUNTIME_DIR); without it the daemon's
        /// unit keeps `<kbf uid>`, which `check-units --installable` refuses.
        #[arg(long)]
        kbf_uid: Option<u32>,
    },
    /// Check rendered units against the host's priorities.
    CheckUnits {
        /// The settings the units were rendered with.
        #[arg(long)]
        config: PathBuf,
        /// The directory render-units wrote.
        #[arg(long)]
        dir: PathBuf,
        /// Also refuse a `<...>` placeholder.
        #[arg(long)]
        installable: bool,
    },
    /// Install the binaries and units, and start kbf-server and kbf-daemon (as root).
    InstallUnits {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Where new kbf-server and kbf-daemon binaries are; without it the installed
        /// ones stay.
        #[arg(long, value_parser = HostPath::new)]
        bin_dir: Option<HostPath>,
        /// Also restart MinIO, so its new priorities take effect now.
        #[arg(long)]
        restart_minio: bool,
        /// Roll back: stop the services and move the four files aside.
        #[arg(long, conflicts_with_all = ["bin_dir", "restart_minio"])]
        uninstall: bool,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Pull the pinned images into the kbf user's Podman store, as that user (as root).
    ImagePull {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// An image instead of KBF_IMAGES (repeat for more): <repo>@sha256:<digest>.
        #[arg(long, value_parser = linux::config::image)]
        image: Vec<ImageRef>,
        /// The platform each image must be built for.
        #[arg(long, default_value = linux::images::PLATFORM)]
        platform: String,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Make, activate and list the build swapfile (as root).
    Swap {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// kubelet runs here and its config sets failSwapOn: false.
        #[arg(long)]
        kubelet_allows_swap: bool,
        /// Roll back: swapoff, drop the fstab line, remove the file.
        #[arg(long, conflicts_with = "kubelet_allows_swap")]
        remove: bool,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum Minio {
    /// Make kbf's buckets, a policy for them and a service account key (see the docs of
    /// `kbf_deploy::minio::buckets`).
    Buckets(BucketsArgs),
    /// Add a NodePort Service for a MinIO Service's S3 port.
    Nodeport(NodePortArgs),
}

#[derive(Args)]
struct BucketsArgs {
    /// The MinIO, http://host[:port].
    #[arg(long, value_parser = minio::endpoint)]
    endpoint: HttpUrl,
    /// A bucket to make (repeat for more): kbf-<lowercase>.
    #[arg(long, required = true, value_parser = KbfName::new)]
    bucket: Vec<KbfName>,
    /// The name of the policy for the buckets.
    #[arg(long, value_parser = KbfName::new)]
    policy: KbfName,
    /// The name of the service account minted for the key file.
    #[arg(long, value_parser = KbfName::new)]
    account: KbfName,
    /// The MinIO client, by absolute path.
    #[arg(long, value_parser = HostPath::new)]
    mc: HostPath,
    /// Where the minted key pair goes (mode 0600); kept when it exists.
    #[arg(long, value_parser = HostPath::new)]
    key_file: Option<HostPath>,
    /// The user who owns the key file (the plan then applies only as root).
    #[arg(long, requires = "key_file")]
    key_owner: Option<String>,
    /// Remove the buckets with their objects, the account, the key file and the policy.
    #[arg(long)]
    remove: bool,
    /// Carry the plan out.
    #[arg(long)]
    apply: bool,
}

#[derive(Args)]
struct NodePortArgs {
    /// kubectl, by absolute path.
    #[arg(long, value_parser = HostPath::new)]
    kubectl: HostPath,
    /// The cluster admin's kubeconfig.
    #[arg(long, value_parser = HostPath::new)]
    kubeconfig: HostPath,
    /// The kubeconfig context; its current one when not given.
    #[arg(long)]
    context: Option<String>,
    /// The MinIO Service's namespace.
    #[arg(long, value_parser = K8sName::new)]
    namespace: K8sName,
    /// The MinIO Service.
    #[arg(long, value_parser = K8sName::new)]
    service: K8sName,
    /// The port every node opens, 30000 to 32767.
    #[arg(long, value_parser = clap::value_parser!(u16).range(30000..=32767))]
    node_port: u16,
    /// Delete the Service (only one carrying the kbf-deploy label).
    #[arg(long)]
    remove: bool,
    /// Carry the plan out.
    #[arg(long)]
    apply: bool,
}

/// The binary's entry point: [`run`] on the host, with the process's environment.
pub fn main(args: Vec<OsString>) -> ExitCode {
    let mut exec = HostExec::new(Root::host());
    run(
        args,
        &mut exec,
        &host_env,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

/// A variable of the process's environment.
fn host_env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// Parse `args` and run the subcommand through `exec`, reading secrets with `env`.
pub fn run<E: Exec>(
    args: Vec<OsString>,
    exec: &mut E,
    env: &dyn Fn(&str) -> Option<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // --help and --version are "errors" that go to stdout and exit 0.
            let text = e.render().to_string();
            let _ = if e.use_stderr() {
                err.write_all(text.as_bytes())
            } else {
                out.write_all(text.as_bytes())
            };
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let result = match cli.kit {
        Kit::Minio(Minio::Buckets(a)) => minio_buckets(a, exec, env, out),
        Kit::Minio(Minio::Nodeport(a)) => minio_nodeport(a, exec, out),
        Kit::Linux(l) => return linux_kit(l, exec, out, err),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The `linux` kit: 0 done, 1 a check found a broken rule, 2 refused.
fn linux_kit<E: Exec>(
    l: Linux,
    exec: &mut E,
    mut out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = match l {
        Linux::RenderUnits {
            config,
            out: dir,
            kbf_uid,
        } => linux::render_units(&config, &dir, kbf_uid).map(|line| (vec![line], 0)),
        Linux::CheckUnits {
            config,
            dir,
            installable,
        } => linux::check_units(&config, &dir, installable).map(|report| {
            let code = u8::from(!report.failures.is_empty());
            let mut lines: Vec<String> = report.failures.iter().map(ToString::to_string).collect();
            lines.push(report.summary);
            (lines, code)
        }),
        Linux::InstallUnits {
            config,
            bin_dir,
            restart_minio,
            uninstall,
            apply,
        } => {
            let done = LinuxConfig::load(&config)
                .map_err(HostError::from)
                .and_then(|config| {
                    if uninstall {
                        install::run_uninstall(exec, &config, &stamp(), mode(apply), &mut out)
                    } else {
                        let a = install::Install {
                            config,
                            bin_dir,
                            restart_minio,
                        };
                        install::run(exec, &a, mode(apply), &mut out)
                    }
                });
            return host_verb(done, err);
        }
        Linux::ImagePull {
            config,
            image,
            platform,
            apply,
        } => {
            let done = LinuxConfig::load(&config)
                .map_err(HostError::from)
                .and_then(|config| {
                    let a = images::Pull {
                        config,
                        images: image,
                        platform,
                    };
                    images::run(exec, &a, mode(apply), &mut out)
                });
            return host_verb(done, err);
        }
        Linux::Swap {
            config,
            kubelet_allows_swap,
            remove,
            apply,
        } => {
            let done = LinuxConfig::load(&config)
                .map_err(HostError::from)
                .and_then(|config| {
                    let a = swap::Swap {
                        config,
                        kubelet_allows_swap,
                        remove,
                    };
                    swap::run(exec, &a, mode(apply), &mut out)
                });
            return host_verb(done, err);
        }
    };
    match result {
        Ok((lines, code)) => {
            for line in lines {
                // A closed stdout (`| head`) is not worth a panic; the exit code still says it.
                let _ = writeln!(out, "{line}");
            }
            ExitCode::from(code)
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

/// A host-changing `linux` verb's exit: 0 done, 1 a step failed (or the check after
/// an apply did), 2 refused.
fn host_verb(done: Result<(), HostError>, err: &mut dyn Write) -> ExitCode {
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e @ HostError::Deploy(kbf_host::HostError::Step { .. })) => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
        Err(e @ HostError::AfterApply(_)) => {
            let _ = writeln!(err, "FAIL {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

/// The directory name `install-units --uninstall` moves the files into: seconds since
/// the Unix epoch.
fn stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .to_string()
}

fn mode(apply: bool) -> Mode {
    if apply { Mode::Apply } else { Mode::DryRun }
}

fn minio_buckets<E: Exec>(
    a: BucketsArgs,
    exec: &mut E,
    env: &dyn Fn(&str) -> Option<OsString>,
    mut out: &mut dyn Write,
) -> Result<(), MinioError> {
    let admin = Admin {
        user: secret(env, "MINIO_ROOT_USER")?,
        password: secret(env, "MINIO_ROOT_PASSWORD")?,
    };
    let scratch = Scratch::new()?;
    let args = buckets::Buckets {
        endpoint: a.endpoint,
        buckets: a.bucket,
        policy: a.policy,
        account: a.account,
        key_file: a.key_file,
        key_owner: a.key_owner,
        mc: a.mc,
        mc_config_dir: scratch.path(),
        remove: a.remove,
    };
    let plan = buckets::plan(&*exec, &args, &admin, &mut out)?;
    execute(&plan, args.run_as(), mode(a.apply), exec, &mut out)?;
    Ok(())
}

fn minio_nodeport<E: Exec>(
    a: NodePortArgs,
    exec: &mut E,
    mut out: &mut dyn Write,
) -> Result<(), MinioError> {
    let scratch = Scratch::new()?;
    let args = nodeport::NodePort {
        kubectl: a.kubectl,
        kubeconfig: a.kubeconfig,
        context: a.context,
        cache_dir: scratch.path(),
        namespace: a.namespace,
        service: a.service,
        node_port: a.node_port,
        remove: a.remove,
    };
    let plan = nodeport::plan(&*exec, &args, &mut out)?;
    execute(&plan, RunAs::User, mode(a.apply), exec, &mut out)?;
    Ok(())
}

/// A secret from the environment: set, UTF-8 and not empty.
fn secret(env: &dyn Fn(&str) -> Option<OsString>, key: &'static str) -> Result<String, MinioError> {
    env(key)
        .and_then(|v| v.into_string().ok())
        .filter(|v| !v.is_empty())
        .ok_or(MinioError::NoAdmin(key))
}

/// A private directory (mode 0700) for a tool's state, removed when dropped.
struct Scratch(PathBuf);

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

impl Scratch {
    fn new() -> Result<Scratch, MinioError> {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kbf-deploy.{}.{n}", std::process::id()));
        DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(MinioError::Scratch)?;
        Ok(Scratch(dir))
    }

    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
