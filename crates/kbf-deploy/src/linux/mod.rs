//! The `linux` kit: a Linux host's swapfile, a kbf-server's metadata backup, and a
//! certificate check. (The systemd units of the `kbf` modes are the packaged files in
//! `packaging/linux/units/`, with their flags in `/etc/kbf/kbf.toml`: `kbf-installer`
//! installs them, and `kbf preflight` checks the host reads they depend on.)
//!
//! Two verbs change the host, as root. Each checks the host first and prints its plan
//! as `DRY-RUN:` lines; `--apply` carries it out, and a re-run after an apply plans
//! nothing. They exit 0 done (or nothing to do), 1 a step or the check after an apply
//! failed, 2 refused (a `REFUSED:` line on stderr).
//! - `kbf-deploy linux swap --config FILE [--kubelet-allows-swap] [--remove] [--apply]`
//!   ([`swap`]): the swapfile, active and in `/etc/fstab`.
//! - `kbf-deploy linux meta-backup --meta-dir DIR [--hot-dir DIR] --out FILE [--apply]`
//!   copies kbf-server's metadata log into one archive in a brief stop (as root);
//!   `linux meta-verify --archive FILE --into DIR` extracts and checks one ([`meta`]).
//!
//! `kbf-deploy linux verify-cert --row ROW --cert FILE --ca-cert FILE [--node-id ID]`
//! ([`verify_cert`], over kbf-ca's checks) checks, read only, that a certificate is its
//! row of the farm's certificate table: its one role, its extended key usage, its chain to the CA.
//!
//! `swap` reads two keys of a `KEY=VALUE` file (`templates/linux/swap.example`); the keys of
//! the retired unit verbs' node settings file are ignored with a warning ([`config::KEYS`]). [`host`] holds what `swap` and the `mdm` kit share of
//! the host.

pub mod config;
pub mod host;
pub mod meta;
pub mod swap;
pub mod verify_cert;

pub use config::{SwapConfig, SwapConfigError};
pub use host::HostError;
