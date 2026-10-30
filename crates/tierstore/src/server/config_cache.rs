//! The build-node shape of the configuration file: `[local_cache]` and `[cold_store]` and no
//! `[hot_store]`, a read cache in front of one upstream (see [`local_cache`](super::local_cache)).
//!
//! ```toml
//! [server]                      # as for a storage node, without backend_check_wait
//! listen          = "127.0.0.1:9400"
//! proxy_id        = "tierstore-cache-1"
//! bucket          = "tierstore"
//! access_key_file = "/etc/tierstore-cache/access.key"
//!
//! [local_cache]
//! path       = "/var/lib/tierstore-cache"
//! layout     = "digest-v1"
//! budget     = "40GiB"
//! high_water = 90
//! low_water  = 75
//! critical   = 95
//! free_floor = 10
//! max_concurrent_fills = 4
//! max_concurrent_keeps = 8
//! write_through        = false  # true: take writes and send them to the upstream
//!
//! [cold_store]                  # the upstream: an S3 endpoint
//! kind             = "s3"
//! bucket           = "tierstore"
//! endpoint         = "http://192.0.2.20:9300"
//! credentials_file = "/etc/tierstore-cache/upstream-read.key"
//!
//! [[policy]]
//! prefix = "cas/b/"
//! cache  = true
//! ```
//!
//! `write_through = true` makes the node take the writes its keys may do (create-only puts,
//! multipart uploads, presigned puts, deletes) and send each to the upstream, which must then be
//! written with a key that may write, and keeps under a `cache = true` policy the objects it
//! puts (`max_concurrent_keeps` of them copied at once): the start tests the upstream's create-only and
//! multipart-conditional answers (see [`cache_cli`](super::cache_cli)). Left out or `false`, the
//! node refuses every write with 403 and its upstream key may be a read-only one.
//!
//! The file is read into a [`CacheSettings`]. It has no flag form. What it refuses, besides what
//! every file does: a `[hot_store]` along with `[local_cache]`, a layout that is not `digest-v1`,
//! `cache = false`, marks out of order, no `[[policy]]` with `cache = true`, a list of more than
//! one `endpoints` in `[cold_store]` (the cache reads from one upstream), and the keys of the
//! storage node that do nothing here: `skip_checks`, the reconciler's keys and
//! `backend_check_wait`.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::config::{
    ConfigError, PerRun, Policy, ServerTable, Size, TierParts, TierTable, check_policies, front_of,
    invalid, parse_size, refuse, refuse_hot_keys, require, tier, within,
};
use super::front::FrontSettings;
use super::local_cache::CacheConfig;

/// The only layout of the cache directory.
const LAYOUT: &str = "digest-v1";

/// A rate in the file: a size and `/s`, such as `50MiB/s`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rate(pub u64);

impl Serialize for Rate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}B/s", self.0))
    }
}

impl<'de> Deserialize<'de> for Rate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(RateVisitor)
    }
}

struct RateVisitor;

impl Visitor<'_> for RateVisitor {
    type Value = Rate;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a rate such as \"50MiB/s\"")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Rate, E> {
        let size = text.strip_suffix("/s").map(parse_size);
        match size {
            Some(Ok(bytes)) => Ok(Rate(bytes)),
            _ => Err(E::custom("not a rate; use a size and /s, as in 50MiB/s")),
        }
    }
}

/// `[local_cache]`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LocalCacheTable {
    /// The cache directory, on the file system that the jobs' scratch is on.
    pub path: std::path::PathBuf,
    /// The layout of the directory: `digest-v1`, the only one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// The bytes the cache is meant to hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Size>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high_water: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_water: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical: Option<u8>,
    /// The percent of the file system that is kept free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_floor: Option<u8>,
    /// The most fills that run at once (1 to 64, default 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_fills: Option<u8>,
    /// The most create-only puts that are copied into the cache at once (1 to 64, default 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_keeps: Option<u8>,
    /// Whether the node takes writes and sends them to the upstream (default `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_through: Option<bool>,
    /// Whether the cache is on: `true`, the default. `false` is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<bool>,
    /// The rate at which a scrubber would re-read the objects. Accepted; this release does not
    /// scrub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrub_rate: Option<Rate>,
}

/// What a build-node file stands for.
#[derive(Debug)]
pub(super) struct CacheSettings {
    /// The S3 front.
    pub front: FrontSettings,
    /// The limit on a request to an S3 upstream.
    pub s3_request_timeout: Duration,
    /// The upstream: the spec and the flags of an S3 bucket.
    pub upstream: TierParts,
    /// The cache.
    pub cache: CacheConfig,
    /// The rate at which a scrubber would run, in bytes a second, if the file gave one.
    pub scrub_rate: Option<u64>,
}

/// The `endpoint` of the upstream: `endpoint`, or the one entry of `endpoints`.
fn upstream_table(mut cold: TierTable) -> Result<TierTable, ConfigError> {
    let Some(list) = cold.endpoints.take() else {
        return Ok(cold);
    };
    if cold.endpoint.is_some() {
        let why = "give endpoint or endpoints, not both";
        return Err(invalid("cold_store.endpoints", why));
    }
    match <[String; 1]>::try_from(list) {
        Ok([only]) => {
            cold.endpoint = Some(only);
            Ok(cold)
        }
        Err(_) => {
            let why = "the cache reads from one upstream: give exactly one endpoint (a list of several is not supported yet)";
            Err(invalid("cold_store.endpoints", why))
        }
    }
}

/// The percent in `value`, or `default`, from `low` to `high`.
fn percent(key: &str, value: Option<u8>, default: u8, low: u8) -> Result<u8, ConfigError> {
    within("local_cache", key, value.unwrap_or(default), low, 100)
}

impl CacheSettings {
    /// The settings of the tables of a build-node file.
    pub(super) fn new(
        server: ServerTable,
        tables: (LocalCacheTable, TierTable),
        policy: &[Policy],
        per_run: PerRun,
    ) -> Result<Self, ConfigError> {
        let (table, cold) = tables;
        check_policies(policy, true)?;
        if !policy.iter().any(|policy| policy.cache) {
            let why = "no policy has cache = true, so nothing would be kept";
            return Err(invalid("policy", why));
        }
        let unused = [("backend_check_wait", server.backend_check_wait.is_some())];
        refuse("server", "not used by a build node", &unused)?;
        refuse_hot_keys(&cold)?;
        let skipping = [("skip_checks", cold.skip_checks.is_some())];
        refuse("cold_store", "not for a build node", &skipping)?;
        let upstream = tier("cold_store", &upstream_table(cold)?)?;
        if table
            .layout
            .as_deref()
            .is_some_and(|layout| layout != LAYOUT)
        {
            let why = format!("the only layout is {LAYOUT:?}");
            return Err(invalid("local_cache.layout", why));
        }
        if table.cache == Some(false) {
            let why = "false turns the cache off; leave out [local_cache] to run without one";
            return Err(invalid("local_cache.cache", why));
        }
        let budget = require("local_cache", "budget", table.budget)?.0;
        let budget = within("local_cache", "budget", budget, 1, u64::MAX)?;
        let high = percent("high_water", table.high_water, 90, 1)?;
        let low = percent("low_water", table.low_water, 75, 1)?;
        let critical = percent("critical", table.critical, 95, 1)?;
        let floor = within(
            "local_cache",
            "free_floor",
            table.free_floor.unwrap_or(10),
            0,
            99,
        )?;
        if low >= high {
            return Err(invalid("local_cache.low_water", "must be below high_water"));
        }
        if high > critical {
            return Err(invalid(
                "local_cache.high_water",
                "must not be above critical",
            ));
        }
        let shared = front_of(server, per_run)?;
        let policies = policy
            .iter()
            .map(|policy| (policy.prefix.clone(), policy.cache))
            .collect();
        let mut cache = CacheConfig::new(table.path, budget, policies);
        cache.high_water = high;
        cache.low_water = low;
        cache.critical = critical;
        cache.free_floor = floor;
        if let Some(fills) = table.max_concurrent_fills {
            let fills = within("local_cache", "max_concurrent_fills", fills, 1, 64)?;
            cache.max_concurrent_fills = usize::from(fills);
        }
        if let Some(keeps) = table.max_concurrent_keeps {
            let keeps = within("local_cache", "max_concurrent_keeps", keeps, 1, 64)?;
            cache.max_concurrent_keeps = usize::from(keeps);
        }
        cache.write_through = table.write_through.unwrap_or(false);
        Ok(Self {
            front: shared.front,
            s3_request_timeout: shared.s3_request_timeout,
            upstream,
            cache,
            scrub_rate: table.scrub_rate.map(|rate| rate.0),
        })
    }
}
