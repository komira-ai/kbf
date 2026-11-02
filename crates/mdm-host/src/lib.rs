//! `mdm-host`: applies [`mdm_core`]'s rendered output to a Linux host.
//!
//! mdm-host is the MDM for enrolling Macs as compute hosts (NanoHUB, a SCEP server, NanoDEP
//! and Caddy as capped systemd services on one Linux host). It is its own binary, beside the
//! update coordinator and apart from the `kbf` binary, and links no other crate of this
//! workspace but `mdm-core` and `host-step` (kbf-integration-test's `mdm_layering` keeps it
//! so). The names it puts on a host (the `js-mdm` unit prefix, user, slice and directories
//! of `config.example`, the `.js-host.tmp` suffix of a file being written) are the ones the
//! settings and the host layer had before the move, so a host installed earlier verifies
//! unchanged. `mdm-core` is the pure
//! half: settings, units, profiles and pins. This crate is the half that touches the host, on
//! top of the host-step layer (`host-step`): every verb checks the host read only, makes a
//! plan of the steps that would change it, prints the plan by default, and carries it out with
//! `--apply`.
//!
//! # Verbs
//!
//! * `mdm-host fetch --dir DIR [--only TOOL]... [--apply]`: the pinned upstream archives,
//!   downloaded over HTTPS as your own user into `DIR` (refused at uid 0: it does not drop
//!   privileges; refused inside a git work tree, links resolved first), each checked against its
//!   sha256 pin. A download that is not the pin is kept as `<file>.rejected` and fails the
//!   run. Nothing downloaded is run.
//! * `mdm-host install --config FILE --bin-dir DIR [--apply]`: as root, the user, the
//!   directories, the binaries unpacked from the checked archives, the secrets (made once
//!   from 32 random bytes, never printed), the SCEP CA (kept if `ca.pem` is there), the
//!   slice, the service units and the Caddyfile, and the services started. The proxy waits
//!   for the TLS certificate.
//! * `mdm-host install --config FILE --uninstall [--apply]`: stops and disables the units and
//!   moves their files to `MDM_STATE/removed/<unix seconds>/`. The state, the TLS certificate,
//!   the secrets, the binaries and the user stay.
//! * `mdm-host tls --config FILE [--apply]`: as root, the TLS certificate from an ACME CA over
//!   DNS-01 by the pinned lego, with the DNS provider's credentials read by the certificate
//!   unit from the root-only `MDM_ETC/dns.env` (never argv), its renewal timer, and the proxy
//!   started once the certificate is there. Only its checks before the plan refuse without a
//!   change: if the certificate is missing after the issuing unit ran, the run fails with the
//!   units written; run it again.
//! * `mdm-host verify --config FILE [--ignore LINE]...`: read only. Health checks (units
//!   active, listeners, certificate lifetime, modes), then one line per installed file
//!   compared with `mdm-core`'s render of the same settings: `same`, `differs` (with the
//!   sha256 of both), `missing` or `wrong` (not a regular file, or not the mode and owner the
//!   install plan gives it). Comment lines are left out of the comparison, and so are the
//!   lines equal to an `--ignore LINE`, on both sides and printed (a host installed by another
//!   tool may word one line differently). World-readable files need no root.
//!
//! Not in this slice: `push-cert` (the APNs push certificate and the enrollment profile),
//! `dep` (Apple Business Manager) and `erase`. The checks that dial the proxy over HTTPS are
//! not either.
//!
//! # Rules every verb keeps
//!
//! * A dry run is the default and changes nothing; `--apply` acts. A rerun finds nothing to
//!   do.
//! * Verbs that read root-only files refuse to run as another user, even for a dry run.
//! * Files are made without following links: a link in a directory the service user owns is
//!   refused, never chmodded, chowned or written through.
//! * A plan names paths, sizes and modes, never contents (its `Debug` form neither). No secret is put on a command line
//!   or printed; the SCEP challenge and API keys are written straight to 0600 root files.
//! * Exit 0 done (or nothing to do), 1 a step, a download or a check failed, 2 refused
//!   (`REFUSED: ...` on stderr).
//! * Settings come from the file `--config` names, never from the environment.
//!
//! # Security notes
//!
//! See `mdm-core`'s "Security notes for the host layer": the enrollment profile is served
//! without authentication on the LAN address and is written 0640 root and the service group;
//! the lexical nesting check of `MDM_STATE`, `MDM_ETC` and `MDM_BIN` does not resolve links,
//! so `install` refuses any of the three that is a symbolic link on the host.

pub mod archive;
mod cli;
mod error;
pub mod fetch;
pub mod install;
pub mod net;
pub mod secret;
pub mod tls;
pub mod verify;

use std::io;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use host_step::{Host, RealHost};

pub use cli::{run, run_with};
pub use error::MdmError;
pub use net::{Download, Net};

/// What a verb needs from the outside: the host, the network, a random source and the time.
pub struct Env<'a> {
    /// The host.
    pub host: &'a dyn Host,
    /// Downloads.
    pub net: &'a dyn Download,
    /// Fills a buffer with random bytes.
    pub random: &'a dyn Fn(&mut [u8]) -> io::Result<()>,
    /// Unix seconds now.
    pub now: u64,
}

/// The kernel's random source.
///
/// # Errors
/// The source could not be read.
pub fn system_random(buf: &mut [u8]) -> io::Result<()> {
    getrandom::getrandom(buf).map_err(io::Error::from)
}

/// Runs the program: reads the arguments, acts on the machine, exits as documented.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let host = RealHost::new();
    let net = Net::new(true);
    let secs = SystemTime::now().duration_since(UNIX_EPOCH);
    let env = Env {
        host: &host,
        net: &net,
        random: &system_random,
        now: secs.map_or(0, |d| d.as_secs()),
    };
    ExitCode::from(run(
        &args,
        &env,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    ))
}
