//! `cargo xtask farm-config --address <A> --instance <I> --tls <true|false> --properties <P>
//! --out <path>`: the farm lane's buckconfig (`.github/workflows/farm.yml`), written from the
//! farm's repository variables, so the workflow joins the tailnet with its own step and needs no
//! shell to make the file.
//!
//! 1. The first lines the verb prints mask the address in the workflow's log
//!    (`::add-mask::<address>`, and the same without its scheme): the logs of a public
//!    repository's runs are public, so nothing printed after them, a refusal included, shows it.
//!    Once the address is checked, the bare host and every address it resolves to are masked too
//!    ([`resolved_masks`]): buck2's connection errors name the resolved address.
//! 2. The flags are checked, and each refusal names its flag: the address, the instance and the
//!    properties are not blank (a fork's run gets no variables, and buck2 would then build on the
//!    runner), no value holds a line break (each value is one line of the file), `--tls` is
//!    exactly `true` or `false`, and the address is `host:port`, with or without a scheme such as
//!    `grpc://`, and a numeric port.
//! 3. `--out` is written, its directory made when missing ([`render`] says what it holds): the
//!    farm's addresses, instance and TLS for buck2's remote execution client, and the worker
//!    property set under buildrules' key `[buildrules_re] linux_x86_64_properties`.
//!
//! The verb prints one line saying which file it wrote, never what it holds.
//!
//! A flag that is missing or not known is a usage error (exit status 2); a refusal is exit 1.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The farm's REAPI address: `host:port`, with or without a scheme such as `grpc://`.
    #[arg(long)]
    pub address: String,
    /// The instance name sent on every request.
    #[arg(long)]
    pub instance: String,
    /// Whether the address is served over TLS: exactly `true` or `false`.
    #[arg(long)]
    pub tls: String,
    /// The worker property set of the Linux x86-64 execution platform, `key=value,...`.
    #[arg(long)]
    pub properties: String,
    /// The buckconfig file to write; its directory is made when missing.
    #[arg(long)]
    pub out: PathBuf,
}

/// Why the configuration was refused or could not be written.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A value is empty or blank.
    #[error(
        "--{flag} is empty: a fork's run gets no variables; refusing, because buck2 would build on \
         the runner"
    )]
    Blank {
        /// The flag.
        flag: &'static str,
    },
    /// A value holds a line break.
    #[error("--{flag} holds a line break: each value is one line of the buckconfig")]
    LineBreak {
        /// The flag.
        flag: &'static str,
    },
    /// `--tls` is neither `true` nor `false`.
    #[error("--tls is {0:?}: it must be exactly `true` or `false`")]
    Tls(String),
    /// The address is not `host:port`, with or without a scheme.
    #[error("--address is not host:port, optionally behind a scheme such as grpc://: {why}")]
    Address {
        /// What is wrong with it.
        why: &'static str,
    },
    /// The file or its directory could not be written.
    #[error("cannot write {path:?}: {why}")]
    Write {
        /// The file.
        path: PathBuf,
        /// The system's reason.
        why: String,
    },
}

/// The values of a run that passed the checks, each without its surrounding whitespace.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings<'a> {
    /// The address.
    pub address: &'a str,
    /// The host of the address, as written (an IPv6 host in its brackets).
    pub host: &'a str,
    /// The port of the address.
    pub port: u16,
    /// The instance name.
    pub instance: &'a str,
    /// Whether the farm is served over TLS.
    pub tls: bool,
    /// The worker property set.
    pub properties: &'a str,
}

/// The address `address` names, without its scheme (`grpc://h:1` is `h:1`).
fn without_scheme(address: &str) -> &str {
    address.split_once("://").map_or(address, |(_, rest)| rest)
}

/// The lines that mask `address` in a workflow's log: the address, and its form without a scheme
/// when it has one. None for a blank address, which there is nothing to mask in.
pub fn masks(address: &str) -> Vec<String> {
    let address = address.trim();
    let bare = without_scheme(address);
    let mut lines = Vec::new();
    if !address.is_empty() {
        lines.push(format!("::add-mask::{address}"));
        if bare != address && !bare.is_empty() {
            lines.push(format!("::add-mask::{bare}"));
        }
    }
    lines
}

/// `value` without its surrounding whitespace, when it is one line and not blank.
fn one_line<'a>(flag: &'static str, value: &'a str) -> Result<&'a str, Error> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Error::Blank { flag });
    }
    if value.contains(['\n', '\r']) {
        return Err(Error::LineBreak { flag });
    }
    Ok(trimmed)
}

/// Whether `scheme` is the scheme of a URL: a letter, then letters, digits, `+`, `-` and `.`.
fn is_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The host and port of `address`, unless it is not `host:port` behind no scheme or a scheme such
/// as `grpc://`.
fn check_address(address: &str) -> Result<(&str, u16), Error> {
    let fail = |why| Err(Error::Address { why });
    if address
        .split_once("://")
        .is_some_and(|(s, _)| !is_scheme(s))
    {
        return fail("the scheme before `://` is not a scheme");
    }
    let Some((host, port)) = without_scheme(address).rsplit_once(':') else {
        return fail("it has no `:port`");
    };
    if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c == '/') {
        return fail("the host is empty, or holds whitespace or a `/`");
    }
    match port.parse::<u16>() {
        Ok(number) if port.bytes().all(|b| b.is_ascii_digit()) => Ok((host, number)),
        _ => fail("the port is not a number from 0 to 65535"),
    }
}

/// The checked settings of `args`.
///
/// # Errors
/// [`Error`] naming the first flag, in the order address, instance, tls, properties, that is
/// refused.
pub fn check(args: &Args) -> Result<Settings<'_>, Error> {
    let address = one_line("address", &args.address)?;
    let (host, port) = check_address(address)?;
    let instance = one_line("instance", &args.instance)?;
    let tls = match args.tls.as_str() {
        "true" => true,
        "false" => false,
        other => return Err(Error::Tls(other.to_owned())),
    };
    let properties = one_line("properties", &args.properties)?;
    Ok(Settings {
        address,
        host,
        port,
        instance,
        tls,
        properties,
    })
}

/// The buckconfig of `settings`.
pub fn render(settings: &Settings<'_>) -> String {
    let Settings {
        address,
        instance,
        tls,
        properties,
        ..
    } = settings;
    // `grpc_keepalive_timeout_secs`: the connection to the farm can be slow (through a tailnet
    // relay it carries a few MB/s for all of a job's uploads together). buck2 PINGs a connection
    // that has read nothing for 30 s, and that PING is queued behind the upload data already
    // written to the connection. With buck2's default of 10 s to answer, a slow but working
    // connection is dropped with every stream on it, and each retry resends its file from the
    // start. 60 s lets the PING through that queue; a connection that is really gone is still
    // dropped, 90 s after its last read.
    //
    // `max_concurrent_uploads_per_action`: at most 4 blobs of one action in flight at once (buck2
    // sends them all at once by default). Fewer streams share the connection, so each blob
    // finishes sooner, and the answers the client reads keep it from needing a PING at all.
    format!(
        "[buck2_re_client]\n  \
         engine_address = {address}\n  \
         action_cache_address = {address}\n  \
         cas_address = {address}\n  \
         instance_name = {instance}\n  \
         tls = {tls}\n  \
         execution_concurrency_limit = 256\n  \
         grpc_keepalive_timeout_secs = 60\n  \
         max_concurrent_uploads_per_action = 4\n\
         [buildrules_re]\n  \
         linux_x86_64_properties = {properties}\n"
    )
}

/// Writes `text` to `path`, making its directory.
fn write(path: &Path, text: &str) -> Result<(), Error> {
    path.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, text))
        .map_err(|e| Error::Write {
            path: path.to_owned(),
            why: e.to_string(),
        })
}

/// What a host name resolves to: the system's resolver in [`run`], a fake in the tests.
pub type Resolve<'a> = &'a dyn Fn(&str, u16) -> Vec<std::net::IpAddr>;

/// The lines that mask what buck2 may print of a checked address besides the address itself: the
/// bare host, and every address the host resolves to, alone and with the port. A connection
/// error names the resolved address (`tcp connect error ... <ip>:<port>`), which the masks of the
/// address as given do not cover when it names a host. A host that does not resolve adds only
/// its own mask; buck2 then fails naming the host, which is masked. Lines `masks` already printed
/// are not repeated.
pub fn resolved_masks(settings: &Settings<'_>, resolve: Resolve<'_>) -> Vec<String> {
    let (address, port) = (settings.address, settings.port);
    let bare = without_scheme(address);
    let mut lines: Vec<String> = Vec::new();
    let mut add = |value: String| {
        let line = format!("::add-mask::{value}");
        if value != address && value != bare && !lines.contains(&line) {
            lines.push(line);
        }
    };
    add(settings.host.to_owned());
    let name = settings.host.trim_start_matches('[').trim_end_matches(']');
    for ip in resolve(name, port) {
        add(ip.to_string());
        add(std::net::SocketAddr::new(ip, port).to_string());
    }
    lines
}

/// The system's resolver: every address `host` resolves to, none when it does not.
fn system_resolve(host: &str, port: u16) -> Vec<std::net::IpAddr> {
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .map(|addrs| addrs.map(|a| a.ip()).collect())
        .unwrap_or_default()
}

/// The verb, with the masking lines written to `out` and host names resolved by `resolve`: the
/// line to print when the file is written.
///
/// # Errors
/// [`Error`] for a refused flag or a file that cannot be written.
pub fn run_with(args: &Args, out: &mut dyn Write, resolve: Resolve<'_>) -> Result<String, Error> {
    for line in masks(&args.address) {
        // A log that cannot be written loses the mask, not the verb's verdict.
        let _ = writeln!(out, "{line}");
    }
    let settings = check(args)?;
    for line in resolved_masks(&settings, resolve) {
        let _ = writeln!(out, "{line}");
    }
    write(&args.out, &render(&settings))?;
    Ok(format!("farm-config: wrote {}", args.out.display()))
}

/// The verb: the line to print when the file is written.
///
/// # Errors
/// [`Error`] for a refused flag or a file that cannot be written.
pub fn run(args: &Args) -> Result<String, Error> {
    run_with(args, &mut std::io::stdout().lock(), &system_resolve)
}

#[cfg(test)]
mod tests;
