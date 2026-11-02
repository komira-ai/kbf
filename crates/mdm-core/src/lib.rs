//! The pure core of mdm-host, the MDM for enrolling Macs as compute hosts: the
//! settings, the profiles a Mac and Apple are given, the systemd units and the Caddyfile
//! of the MDM services, the pinned upstream binaries and the arguments the host layer
//! passes to them.
//!
//! This is a pure crate: no I/O, no process, no clock, no randomness and no hashed
//! collections. Everything is a function from settings text to bytes, so a site's files
//! are the same on every render and can be compared with what is installed. The binary
//! that reads the settings file, fetches the tools and installs the files is mdm-host,
//! which calls this crate. It was ported from the `mdm` module of kbf-deploy (kbf
//! commit `f89ed5c063728e2949c2bcc65c2cda99a313d771`), with the names that module fixed
//! (the unit prefix, the slice, the SCEP CA's subject, the DEP certificate's name, the
//! ADE profile's text, the signature namespace) moved into settings, so the crate holds
//! no name of any one site.
//!
//! # Security notes for the host layer
//!
//! * The enrollment profile carries the SCEP challenge, and the proxy serves the file at
//!   `/enroll` with no authentication, on the LAN address only (the design kbf-deploy
//!   has). Whoever can reach that address can read the challenge, so the LAN must be one
//!   whose members may enroll. The host layer must write the file with mode 0640 or
//!   tighter, owned by root and readable by the service group only.
//! * The check that `MDM_STATE`, `MDM_ETC` and `MDM_BIN` do not equal or nest in each
//!   other is lexical: symbolic links and mounts are not resolved, so the host layer must
//!   not accept a link that makes one of them point into another.
//!
//! # Where to start
//!
//! * [`config::MdmConfig::parse`] and [`acme::Acme::parse`] read a settings file
//!   (`config.example` in this crate documents every key).
//! * [`render::render`] and [`render::render_tls`] give the units and the Caddyfile.
//! * [`profile::enrollment`] and [`profile::AdeProfile`] give the profiles.
//! * [`pins::Pins::built_in`] gives the pinned tools.
//! * [`commands`] gives the SCEP CA's and the DEP certificate's arguments.
//!
//! # Comparing with an installed host
//!
//! [`render::manifest`] prints one `<sha256>  <name>` line per file. With
//! [`render::Comments::Strip`] the comment lines are left out of the hash: they are the
//! only wording that differs from the kbf-deploy templates, so a host installed by
//! kbf-deploy hashes the same as this crate's render of settings that carry the same
//! names, except the slice, whose `Description=` lost the words ` on the dev box`.
//!
//! To compare a host: put the settings file it was installed with in
//! `tests/slice1/site.conf` and, in `tests/slice1/installed.sha256`, the lines
//! `sha256sum` prints for the installed units with comment lines removed (for each file,
//! `grep -v '^[[:space:]]*#' FILE | sha256sum`, then the file's name in place of `-`,
//! two spaces between), the slice's line computed after
//! `sed 's/ on the dev box//'`. Then run
//! `cargo test -p mdm-core --test installed_host -- --ignored`. Nothing depends on
//! that test passing.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod acme;
pub mod commands;
pub mod config;
pub mod https;
pub mod path;
pub mod pins;
pub mod profile;
pub mod render;
pub mod settings;
mod sha1;
pub mod template;
