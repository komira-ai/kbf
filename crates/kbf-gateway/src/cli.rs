//! The command line, and what each command does.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use crate::envoy::{self, Upstream};
use crate::gateway::{self, Config, Error};
use crate::registry::Clock;

/// The longest registration TTL a flag may name: one hour.
const MAX_TTL_MS: u64 = 3_600_000;

/// `kbf-gateway`: the client box's gateway.
#[derive(Debug, Parser)]
#[command(name = "kbf-gateway", version, about)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The gateway's commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Accepts server registrations and serves the leader endpoint to the local Envoy.
    Serve(Serve),
    /// Prints the Envoy bootstrap that goes with the gateway.
    EnvoyConfig(EnvoyConfig),
}

/// Flags of `serve`.
#[derive(Debug, Args)]
pub struct Serve {
    /// Where servers register, over mutual TLS.
    #[arg(long)]
    pub register_listen: SocketAddr,
    /// PEM certificate (chain) of the registration listener.
    #[arg(long)]
    pub tls_cert: PathBuf,
    /// PEM private key of the registration listener.
    #[arg(long)]
    pub tls_key: PathBuf,
    /// PEM CA that callers' certificates must chain to: the farm's. Only server
    /// certificates register (extended key usage serverAuth, one DNS name).
    #[arg(long)]
    pub client_ca: PathBuf,
    /// Where xDS is served to the Envoy on this box: plain text, so a loopback address.
    #[arg(long, default_value = "127.0.0.1:18000")]
    pub xds_listen: SocketAddr,
    /// How long, in milliseconds, a registration lives without another call. Servers
    /// call every two seconds, so the default tolerates two missed calls.
    #[arg(
        long,
        default_value_t = 6_000,
        value_parser = clap::value_parser!(u64).range(1..=MAX_TTL_MS)
    )]
    pub ttl_ms: u64,
}

/// Flags of `envoy-config`.
#[derive(Debug, Args)]
pub struct EnvoyConfig {
    /// Where Envoy serves REAPI to build clients.
    #[arg(long, default_value = "127.0.0.1:8980")]
    pub listen: SocketAddr,
    /// Where the gateway serves xDS (its `--xds-listen`).
    #[arg(long, default_value = "127.0.0.1:18000")]
    pub xds: SocketAddr,
    /// Envoy's admin listener.
    #[arg(long, default_value = "127.0.0.1:9901")]
    pub admin: SocketAddr,
    /// PEM CA the servers' REAPI certificates chain to. Without it Envoy speaks plain
    /// text to the servers.
    #[arg(long, requires = "upstream_san_suffix")]
    pub upstream_ca: Option<PathBuf>,
    /// A DNS name suffix, such as `.farm.example`, that every server certificate has a
    /// name under.
    #[arg(long, requires = "upstream_ca")]
    pub upstream_san_suffix: Option<String>,
}

impl EnvoyConfig {
    /// The bootstrap these flags ask for.
    #[must_use]
    pub fn render(&self) -> String {
        let upstream = match (&self.upstream_ca, &self.upstream_san_suffix) {
            (Some(ca), Some(san_suffix)) => Some(Upstream {
                ca: ca.clone(),
                san_suffix: san_suffix.clone(),
            }),
            _ => None,
        };
        envoy::render(&envoy::Options {
            listen: self.listen,
            xds: self.xds,
            admin: self.admin,
            upstream,
        })
    }
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })
}

impl Serve {
    /// The gateway's configuration, reading the TLS files.
    ///
    /// # Errors
    /// A TLS file cannot be read.
    pub fn config(&self) -> Result<Config, Error> {
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(
                read(&self.tls_cert)?,
                read(&self.tls_key)?,
            ))
            .client_ca_root(Certificate::from_pem(read(&self.client_ca)?));
        Ok(Config {
            register_listen: self.register_listen,
            tls,
            xds_listen: self.xds_listen,
            ttl: Duration::from_millis(self.ttl_ms),
        })
    }
}

/// Installs the SIGINT and SIGTERM handlers now and returns a future that completes on
/// either, so a signal between the start line and the first poll still stops the process
/// cleanly.
fn interrupted() -> std::io::Result<impl std::future::Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    })
}

/// Runs a command. `serve` prints one start line, `kbf-gateway <version> register=<addr>
/// xds=<addr>`, once both listeners are bound, and serves until SIGINT or SIGTERM.
///
/// # Errors
/// A file cannot be read, a listener cannot be bound, or serving fails.
pub fn run(cli: &Cli) -> Result<(), Error> {
    match &cli.command {
        Command::EnvoyConfig(flags) => {
            print!("{}", flags.render());
            Ok(())
        }
        Command::Serve(flags) => {
            let config = flags.config()?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(Error::Runtime)?;
            runtime.block_on(async {
                let stop = interrupted().map_err(Error::Runtime)?;
                let bound = gateway::bind(config, Clock::system(), stop)?;
                println!(
                    "kbf-gateway {} register={} xds={}",
                    env!("CARGO_PKG_VERSION"),
                    bound.registration,
                    bound.xds
                );
                bound.serve().await
            })
        }
    }
}
