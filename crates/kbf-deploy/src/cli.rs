//! The `kbf-deploy` command line.
//!
//! ```text
//! kbf-deploy minio buckets --endpoint URL --bucket NAME... --policy NAME --account NAME
//!                          --mc PATH [--key-file PATH [--key-owner USER]] [--remove] [--apply]
//! kbf-deploy minio nodeport --kubectl PATH --kubeconfig PATH [--context NAME]
//!                           --namespace NS --service NAME --node-port N [--remove] [--apply]
//! kbf-deploy linux swap --config FILE [--kubelet-allows-swap] [--remove] [--apply]
//! kbf-deploy linux meta-backup --meta-dir DIR [--hot-dir DIR] --out FILE [--metrics URL]
//!                              [--unit NAME] [--ready-secs N] [--apply]
//! kbf-deploy linux meta-verify --archive FILE --into DIR
//! kbf-deploy mdm fetch --dir DIR [--only TOOL]... [--apply]
//! kbf-deploy mdm install --config FILE --bin-dir DIR [--apply]
//! kbf-deploy mdm install --config FILE --uninstall [--apply]
//! kbf-deploy mdm tls --config FILE [--apply]
//! kbf-deploy mdm tls --config FILE --check-godaddy
//! kbf-deploy mdm push-cert --config FILE --cert PEM --key PEM [--apply]
//! kbf-deploy mdm push-cert --config FILE --profile-only [--topic TOPIC] [--apply]
//! kbf-deploy mdm dep get-cert --config FILE --out FILE [--apply]
//! kbf-deploy mdm dep upload-token --config FILE --token FILE [--apply]
//! kbf-deploy mdm dep define-profile --config FILE [--apply]
//! kbf-deploy mdm dep assign --config FILE --serial SERIAL... [--apply]
//! kbf-deploy mdm verify --config FILE [--via ADDR:PORT]
//! kbf-deploy mdm erase-request --serial SERIAL --reason TEXT [--lease ID] --key KEY
//!                           --out FILE [--valid-for MINUTES] [--ssh-keygen PATH] [--apply]
//! kbf-deploy pilot server-install --config FILE --bin FILE [--bind ADDR] [--pki DIR]
//!                                 [--s3-env-file FILE] [--apply]
//! kbf-deploy pilot server-install --config FILE --uninstall [--apply]
//! kbf-deploy pilot buckconfig --config FILE --worktree DIR [--remove] [--apply]
//! kbf-deploy log (--log-dir DIR | --hot-store DIR) [--lines N]
//! ```
//!
//! Bad flags exit 2 for every subcommand. The `minio` subcommands change a host: without
//! `--apply` they print their plan as `DRY-RUN:` lines and change nothing, and they exit
//! 0 done (or nothing to do) or 1 refused or failed. `linux swap` changes the
//! host as root, plans by default as the `minio` subcommands do, and exits 0 done (or
//! nothing to do), 1 a step or the check after an apply failed (`FAIL ...`), or 2
//! refused (`REFUSED: ...` on stderr). `linux meta-backup` and `meta-verify` exit 0
//! done, 1 a failure after the stop (the unit restarted) or an archive that does not
//! verify (`FAIL ...` on stderr), or 2 refused ([`crate::linux::meta`]).
//! The `mdm` verbs exit as the `linux` host verbs do; `mdm verify` exits 1 when a check
//! fails, `mdm dep assign` when Apple did not assign every device, and `mdm tls
//! --check-godaddy` when the account may not use the API ([`crate::mdm`]).
//! The `pilot` verbs run as the operator's own user, plan by default as the `minio`
//! subcommands do, and exit 0 done (or nothing to do), 1 a step failed or kbf-server
//! never reported ready after its start, or 2 refused (`REFUSED: ...` on stderr)
//! ([`crate::pilot`]).
//! `log` only reads: it prints the last `--lines` (default 100) lines of a kbf-server's
//! durable log, across `kbf-server.log` and its rotated files, oldest first, and exits 0,
//! or 2 refused (`REFUSED: ...` on stderr: no such directory, or no log in it)
//! ([`crate::log`]).
//! `minio buckets` reads the MinIO admin key pair from `MINIO_ROOT_USER` and
//! `MINIO_ROOT_PASSWORD` in the environment (secrets are never flags: argv is
//! world-readable).

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};

use crate::https::Client;
use crate::linux::{self, HostError, SwapConfig, meta, swap};
use crate::log;
use crate::mdm::dep::{Dep, Serial};
use crate::mdm::erase::{self, EraseArgs, Made};
use crate::mdm::fetch::Fetch;
use crate::mdm::install::Install;
use crate::mdm::profile::Topic;
use crate::mdm::push_cert::{self, PushCert, With};
use crate::mdm::tls::Acme;
use crate::mdm::verify::Verify;
use crate::mdm::{self, MdmConfig, MdmError, Pins, Tool};
use crate::minio::{self, Admin, K8sName, KbfName, MinioError, buckets, nodeport};
use crate::pilot;
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
    /// The host side of a commit-confirm deploy: the releases under the installer's tree
    /// (/var/lib/kbf-installer on Linux) and the state.
    #[command(subcommand)]
    Host(crate::host::Host),
    /// A MinIO for kbf: its buckets and key, and a NodePort for its S3 port.
    #[command(subcommand)]
    Minio(Minio),
    /// A Linux node: the build swapfile, the metadata backup and the certificate check.
    #[command(subcommand)]
    Linux(Linux),
    /// An Apple MDM on one Linux host: NanoHUB, the SCEP server, NanoDEP and an HTTPS
    /// proxy.
    #[command(subcommand)]
    Mdm(Mdm),
    /// A pilot kbf-server as a systemd user service, and the buck2 overlay that points
    /// one worktree at it.
    #[command(subcommand)]
    Pilot(pilot::cli::Pilot),
    /// The last lines of a kbf-server's durable log, across its rotated files, oldest
    /// first. Only reads.
    Log(log::LogArgs),
}

#[derive(Subcommand)]
enum Linux {
    /// Back up kbf-server's metadata log in a brief stop (as root): one tar file with
    /// a sha256 manifest; prints the downtime.
    MetaBackup(meta::cli::BackupArgs),
    /// Extract a meta-backup archive into an empty directory and check every file.
    MetaVerify(meta::cli::VerifyArgs),
    /// Check, read only, that a certificate is its row of the farm's certificate table:
    /// its one role, its extended key usage and its chain to the CA. The frontend's
    /// REAPI certificate is not the farm CA's and is no row.
    VerifyCert(linux::verify_cert::VerifyArgs),
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
enum Mdm {
    /// Download the pinned upstream archives into a directory, as yourself.
    Fetch {
        /// Where the archives go: absolute, outside every git work tree.
        #[arg(long)]
        dir: PathBuf,
        /// Only this tool (repeat for more); every pinned tool when not given.
        #[arg(long, value_parser = Tool::named)]
        only: Vec<Tool>,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Install the MDM services from the fetched archives (as root).
    Install {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Where `mdm fetch` put the archives.
        #[arg(long, value_parser = HostPath::new)]
        bin_dir: Option<HostPath>,
        /// Roll back: stop the services and move the unit files aside.
        #[arg(long, conflicts_with = "bin_dir")]
        uninstall: bool,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Get the MDM's TLS certificate over DNS-01 with lego, renewed by a timer, and
    /// start the proxy (as root).
    Tls {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Only ask GoDaddy's DNS API whether this account may use it (read only).
        #[arg(long, conflicts_with = "apply")]
        check_godaddy: bool,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Give NanoHUB the APNs push certificate, and write the enrollment profile the
    /// proxy serves (as root).
    PushCert {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// The push certificate (PEM) from the Apple Push Certificates Portal.
        #[arg(long, value_parser = HostPath::new, required_unless_present = "profile_only")]
        cert: Option<HostPath>,
        /// Its private key (PEM, unencrypted, mode 0600).
        #[arg(long, value_parser = HostPath::new, required_unless_present = "profile_only")]
        key: Option<HostPath>,
        /// Write the profile only, for --topic or the recorded topic.
        #[arg(long, conflicts_with_all = ["cert", "key"])]
        profile_only: bool,
        /// The push topic, com.apple.mgmt.External.<uuid> (with --profile-only).
        #[arg(long, requires = "profile_only", value_parser = |s: &str| Topic::new(s).map_err(|e| e.to_string()))]
        topic: Option<Topic>,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Link NanoDEP to Apple Business Manager and give it the ADE profile (as root).
    #[command(subcommand)]
    Dep(DepVerb),
    /// Check that the MDM is up and exposed only as intended (read only; use sudo).
    Verify {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Dial the proxy here instead of MDM_BIND:443 (a forwarded port, say); it must
        /// still prove MDM_DNS_NAME.
        #[arg(long)]
        via: Option<SocketAddr>,
    },
    /// Write an erase request for one Mac and sign it with your security key, on your
    /// own machine (as yourself, not root); check it as the gate will.
    EraseRequest {
        /// The Mac's serial number.
        #[arg(long)]
        serial: String,
        /// Why, in plain words (the gate's alerts quote it).
        #[arg(long)]
        reason: String,
        /// Hold the erase for this privileged lease instead of erasing now.
        #[arg(long)]
        lease: Option<String>,
        /// The security key's key file (`ssh-keygen -f`), with KEY.pub beside it.
        #[arg(long, value_parser = HostPath::new)]
        key: HostPath,
        /// Where the request goes; the signature goes to FILE.sig. Neither may exist.
        #[arg(long, value_parser = HostPath::new)]
        out: HostPath,
        /// Minutes from now the gate may accept the request (1 to 60).
        #[arg(long, default_value_t = 15)]
        valid_for: u32,
        /// The ssh-keygen to sign with (one with security key support).
        #[arg(long, value_parser = HostPath::new, default_value = erase::SSH_KEYGEN)]
        ssh_keygen: HostPath,
        /// Sign: write the request and its signature.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum DepVerb {
    /// Have NanoDEP make its token key pair; write the certificate to upload in ABM.
    GetCert {
        #[command(flatten)]
        with: DepArgs,
        /// Where the certificate goes: absolute, outside every git work tree.
        #[arg(long, value_parser = HostPath::new)]
        out: HostPath,
    },
    /// Give NanoDEP the server token (.p7m) ABM made for that certificate.
    UploadToken {
        #[command(flatten)]
        with: DepArgs,
        /// The token: absolute, outside every git work tree.
        #[arg(long, value_parser = HostPath::new)]
        token: HostPath,
    },
    /// Define the ADE profile at Apple and make it the one new devices get.
    DefineProfile {
        #[command(flatten)]
        with: DepArgs,
    },
    /// Assign the ADE profile to devices already in ABM.
    Assign {
        #[command(flatten)]
        with: DepArgs,
        /// A device's serial number (repeat for more).
        #[arg(long, required = true, value_parser = Serial::new)]
        serial: Vec<Serial>,
    },
}

#[derive(Args)]
struct DepArgs {
    /// The kit's settings, KEY=VALUE lines.
    #[arg(long)]
    config: PathBuf,
    /// Carry the plan out.
    #[arg(long)]
    apply: bool,
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

/// Parse `args` and run the subcommand through `exec`, reading secrets with `env`, at
/// the host's clock.
pub fn run<E: Exec>(
    args: Vec<OsString>,
    exec: &mut E,
    env: &dyn Fn(&str) -> Option<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    run_at(args, exec, env, SystemTime::now(), out, err)
}

/// [`run`] with the instant it stamps rollback directories, erase requests and
/// certificate checks with given, so a test can fix it.
pub fn run_at<E: Exec>(
    args: Vec<OsString>,
    exec: &mut E,
    env: &dyn Fn(&str) -> Option<OsString>,
    now: SystemTime,
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
        Kit::Host(h) => return crate::host::run(h, out, err),
        Kit::Minio(Minio::Buckets(a)) => minio_buckets(a, exec, env, out),
        Kit::Minio(Minio::Nodeport(a)) => minio_nodeport(a, exec, out),
        Kit::Linux(l) => return linux_kit(l, exec, out, err),
        Kit::Mdm(m) => return mdm_kit(m, exec, now, out, err),
        Kit::Log(a) => return log::run(&a, out, err),
        Kit::Pilot(p) => {
            return pilot::cli::run(p, exec, env, now, out, err, &pilot::cli::sleep_a_second);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The `linux` kit: 0 done, 1 a step failed, 2 refused.
fn linux_kit<E: Exec>(
    l: Linux,
    exec: &mut E,
    mut out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match l {
        Linux::MetaBackup(a) => meta::cli::backup(a, exec, &meta::metrics::http_get, out, err),
        Linux::MetaVerify(a) => meta::cli::verify(&a, out, err),
        Linux::VerifyCert(a) => linux::verify_cert::cli(&a, out, err),
        Linux::Swap {
            config,
            kubelet_allows_swap,
            remove,
            apply,
        } => {
            let done = SwapConfig::load(&config)
                .map_err(HostError::from)
                .and_then(|config| {
                    if let Some(warning) = config.retired_warning() {
                        let _ = writeln!(err, "{warning}");
                    }
                    let a = swap::Swap {
                        config,
                        kubelet_allows_swap,
                        remove,
                    };
                    swap::run(exec, &a, mode(apply), &mut out)
                });
            host_verb(done, err)
        }
    }
}

/// How long a download may stall, and a check through the proxy may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// The `mdm` kit: 0 done (or every check passed), 1 a step, download or check failed,
/// 2 refused.
fn mdm_kit<E: Exec>(
    m: Mdm,
    exec: &mut E,
    now: SystemTime,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let done =
        match m {
            Mdm::Fetch { dir, only, apply } => Client::system(FETCH_TIMEOUT)
                .map_err(MdmError::from)
                .and_then(|client| {
                    let f = Fetch { dir, only };
                    mdm::fetch::run(
                        &f,
                        &Pins::built_in(),
                        exec.euid(),
                        &client,
                        mode(apply),
                        out,
                    )
                })
                .map(|()| true),
            Mdm::Install {
                config,
                bin_dir,
                uninstall,
                apply,
            } => MdmConfig::load(&config)
                .map_err(MdmError::from)
                .and_then(|config| match (uninstall, bin_dir) {
                    (true, _) => {
                        mdm::install::run_uninstall(exec, &config, &stamp(now), mode(apply), out)
                    }
                    (false, Some(bin_dir)) => {
                        let a = Install { config, bin_dir };
                        mdm::install::run(exec, &a, &Pins::built_in(), mode(apply), out)
                    }
                    (false, None) => Err(MdmError::NoBinDir),
                })
                .map(|()| true),
            Mdm::Tls {
                config,
                check_godaddy,
                apply,
            } => MdmConfig::load(&config)
                .map_err(MdmError::from)
                .and_then(|c| {
                    if check_godaddy {
                        let client = Client::system(VERIFY_TIMEOUT)?;
                        return mdm::tls::check_godaddy(&*exec, &c, &client, None, out);
                    }
                    let acme = Acme::load(&config)?;
                    mdm::tls::run(exec, &c, &acme, mode(apply), out).map(|()| true)
                }),
            Mdm::PushCert {
                config,
                cert,
                key,
                profile_only: _,
                topic,
                apply,
            } => MdmConfig::load(&config)
                .map_err(MdmError::from)
                .and_then(|config| {
                    let with = match (cert, key) {
                        (Some(cert), Some(key)) => With::Pair { cert, key },
                        _ => With::ProfileOnly(topic),
                    };
                    let a = PushCert { config, with };
                    push_cert::run(exec, &a, now, mode(apply), out)
                })
                .map(|()| true),
            Mdm::Dep(verb) => {
                let (with, d) = match verb {
                    DepVerb::GetCert { with, out } => (with, Dep::GetCert { out }),
                    DepVerb::UploadToken { with, token } => (with, Dep::UploadToken { token }),
                    DepVerb::DefineProfile { with } => (with, Dep::DefineProfile),
                    DepVerb::Assign { with, serial } => (with, Dep::Assign { serials: serial }),
                };
                MdmConfig::load(&with.config)
                    .map_err(MdmError::from)
                    .and_then(|c| mdm::dep::run(exec, &c, &d, mode(with.apply), out))
            }
            Mdm::Verify { config, via } => MdmConfig::load(&config)
                .map_err(MdmError::from)
                .and_then(|config| {
                    let client = Client::system(VERIFY_TIMEOUT)?;
                    let v = Verify {
                        proxy: via.unwrap_or(SocketAddr::from((config.bind, 443))),
                        config: &config,
                        client: &client,
                        now,
                    };
                    mdm::verify::run(&*exec, &v, out)
                }),
            Mdm::EraseRequest {
                serial,
                reason,
                lease,
                key,
                out: file,
                valid_for,
                ssh_keygen,
                apply,
            } => erase::random_nonce()
                .and_then(|nonce| {
                    let a = EraseArgs {
                        serial,
                        lease,
                        reason,
                        key,
                        out: file,
                        valid_for,
                        ssh_keygen,
                    };
                    let now = i64::try_from(epoch_secs(now)).unwrap_or(i64::MAX);
                    erase::run(exec, &a, Made { now, nonce }, mode(apply), out)
                })
                .map(|()| true),
        };
    match done {
        Ok(passed) => ExitCode::from(u8::from(!passed)),
        Err(e) if e.exit_code() == 1 => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

/// A host-changing `linux` verb's exit: 0 done, 1 a step failed, 2 refused.
fn host_verb(done: Result<(), HostError>, err: &mut dyn Write) -> ExitCode {
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e @ HostError::Deploy(kbf_host::HostError::Step { .. })) => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

/// The directory name `mdm install --uninstall` and `pilot server-install --uninstall`
/// move the files into: seconds since the Unix epoch.
pub(crate) fn stamp(now: SystemTime) -> String {
    epoch_secs(now).to_string()
}

/// Seconds since the epoch at `at` (0 before it).
fn epoch_secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
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
    let scratch = Scratch::new().map_err(MinioError::Scratch)?;
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
    let scratch = Scratch::new().map_err(MinioError::Scratch)?;
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
    fn new() -> io::Result<Scratch> {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kbf-deploy.{}.{n}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&dir)?;
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
