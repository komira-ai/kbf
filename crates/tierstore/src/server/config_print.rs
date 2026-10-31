//! `--print-config`: the configuration file that is equivalent to the flags given.
//!
//! It is the way from a unit that lists flags to a file. The file names the key files by path,
//! as the flags do, and holds no key; it is loaded back by the loader of [`config`](super::config)
//! before it is returned, so what is printed is a file that loads, to the settings the flags made.

use std::path::Path;

use super::cli::Cli;
use super::config::{
    ConfigError, Dur, File, Kind, OciTable, PerRun, ServerTable, Size, TierTable, from_text,
};

/// What the file starts with.
const HEADER: &str = "\
# The configuration file equivalent to the flags that `tierstore --print-config` was given.
# Start the server with `tierstore --config <this file>` and without the flags it replaces;
# only --drain-lb-wait and --drain-deadline stay flags. The key files are named by path.

";

/// The flags of a tier that are for an S3 bucket.
#[derive(Clone, Copy)]
struct S3Flags<'a> {
    endpoint: Option<&'a str>,
    region: Option<&'a str>,
    credentials_file: Option<&'a Path>,
}

/// The table of the tier that `spec` names (`memory`, `dir:<path>` or `s3:<bucket>`), with the
/// S3 flags the tier was given. `flag` names the flag in a refusal.
fn tier_table(flag: &str, spec: &str, s3: S3Flags<'_>) -> Result<TierTable, ConfigError> {
    let mut table = TierTable {
        endpoint: s3.endpoint.map(str::to_owned),
        region: s3.region.map(str::to_owned),
        credentials_file: s3.credentials_file.map(Path::to_path_buf),
        ..TierTable::default()
    };
    match (spec, spec.strip_prefix("dir:"), spec.strip_prefix("s3:")) {
        ("memory", ..) => table.kind = Kind::Memory,
        (_, Some(path), _) => {
            table.kind = Kind::Dir;
            table.path = Some(path.into());
        }
        (_, _, Some(bucket)) => {
            table.kind = Kind::S3;
            table.bucket = Some(bucket.to_owned());
        }
        _ => {
            let why = "only `memory`, `dir:<path>` and `s3:<bucket>` are tiers";
            return Err(ConfigError::Invalid {
                key: flag.to_owned(),
                why: format!("{spec:?}: {why}"),
            });
        }
    }
    Ok(table)
}

/// The text of the file for the settings in `cli`.
///
/// # Errors
/// A [`ConfigError`] when the flags cannot be written as a file that loads: a tier that is none
/// of the three kinds, S3 flags given to a tier that is not an S3 bucket, an S3 tier with no
/// endpoint or credentials file, or a number TOML cannot hold.
pub(super) fn render(cli: &Cli) -> Result<String, ConfigError> {
    let hot_s3 = S3Flags {
        endpoint: cli.hot_endpoint.as_deref(),
        region: cli.hot_region.as_deref(),
        credentials_file: cli.hot_credentials_file.as_deref(),
    };
    let mut hot = tier_table("--hot", &cli.hot, hot_s3)?;
    hot.budget = Some(Size(cli.hot_bytes));
    hot.high_water = Some(cli.high_water);
    hot.low_water = Some(cli.low_water);
    hot.critical = Some(cli.critical);
    hot.copy_parallel = Some(cli.copy_parallel);
    hot.lease_ttl = Some(Dur(cli.lease_ttl));
    hot.reconcile_interval = Some(Dur(cli.reconcile_interval));
    hot.usage_stale_after = Some(Dur(cli.usage_stale_after));
    hot.journal_linger = Some(Dur(cli.journal_linger));
    hot.journal_retention = Some(Dur(cli.journal_retention));
    let cold_s3 = S3Flags {
        endpoint: cli.cold_endpoint.as_deref(),
        region: cli.cold_region.as_deref(),
        credentials_file: cli.cold_credentials_file.as_deref(),
    };
    let mut cold = tier_table("--cold", &cli.cold, cold_s3)?;
    let skipped = cli
        .skip_backend_check
        .iter()
        .map(|skipped| format!("{}={}", skipped.check, skipped.reason))
        .collect();
    cold.skip_checks = Some(skipped);
    let server = ServerTable {
        listen: cli.listen,
        proxy_id: cli.proxy_id.clone(),
        bucket: cli.bucket.clone(),
        region: Some(cli.region.clone()),
        access_key_file: cli.access_key_file.clone(),
        read_only_key_files: cli.read_only_key_file.clone(),
        presign_key_files: cli.presign_key_file.clone(),
        gc_key_files: cli.gc_key_file.clone(),
        scoped_key_files: cli.scoped_key_file.clone(),
        presign_max_expires: Some(Dur(cli.presign_max_expires)),
        max_request_bytes: Some(cli.max_request_bytes),
        max_concurrent_puts: Some(cli.max_concurrent_puts),
        put_permit_wait: Some(Dur(cli.put_permit_wait)),
        put_min_bytes_per_sec: Some(cli.put_min_bytes_per_sec),
        put_rate_grace: Some(Dur(cli.put_rate_grace)),
        body_idle_timeout: Some(Dur(cli.body_idle_timeout)),
        s3_request_timeout: Some(Dur(cli.s3_request_timeout)),
        backend_check_wait: Some(Dur(cli.backend_check_wait)),
    };
    // The flags that need `--oci-listen` cannot be given without it, so the table is the front.
    let oci = cli.oci_listen.map(|listen| OciTable {
        listen: Some(listen),
        tls_cert_file: cli.oci_tls_cert.clone(),
        tls_key_file: cli.oci_tls_key.clone(),
        prefix: cli.oci_prefix.clone(),
        max_connections: cli.oci_max_connections,
        connection_lifetime: cli.oci_connection_lifetime.map(Dur),
    });
    let file = File {
        server,
        hot_store: Some(hot),
        cold_store: Some(cold),
        oci,
        local_cache: None,
        policy: Vec::new(),
    };
    let text = toml::to_string(&file)?;
    let per_run = PerRun {
        drain_lb_wait: cli.drain_lb_wait,
        drain_deadline: cli.drain_deadline,
    };
    from_text(&text, per_run)?;
    Ok(format!("{HEADER}{text}"))
}

#[cfg(test)]
#[path = "config_print_tests.rs"]
mod tests;
