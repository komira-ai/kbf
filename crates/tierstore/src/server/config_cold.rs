//! The pass-through shape of the configuration file: `[server]` and `[cold_store]` and nothing
//! else of the tiers, a front that sends every call straight to the cold store.
//!
//! ```toml
//! [server]                      # as for a storage node
//! listen          = "127.0.0.1:9300"
//! proxy_id        = "tierstore-pass-1"
//! bucket          = "tierstore"
//! access_key_file = "/etc/tierstore/access.key"
//!
//! [cold_store]
//! kind             = "s3"
//! bucket           = "tierstore"
//! endpoint         = "http://192.0.2.20:9000"
//! credentials_file = "/etc/tierstore/cold.credentials"
//! ```
//!
//! The file is read into a [`ColdSettings`]. It has no flag form. Besides what every file
//! refuses, this shape refuses: `skip_checks` (the one check that can be skipped is the one that
//! shows the cold store keeps an object that exists when a multipart upload of the same key is
//! completed, and this front answers every create-only call from the cold store alone, so a store
//! that cannot pass it cannot be put behind it), `endpoints` (it has one cold store), and the keys
//! that belong to `[hot_store]`. `[[policy]]` is read and checked as on a storage node and has no
//! effect; `cache = true` is refused, there being no cache.

use std::time::Duration;

use super::config::{
    ConfigError, PerRun, Policy, ServerTable, TierParts, TierTable, check_policies, front_of,
    invalid, refuse, refuse_hot_keys, tier,
};
use super::front::FrontSettings;

/// What a pass-through file stands for.
#[derive(Debug)]
pub(super) struct ColdSettings {
    /// The S3 front.
    pub front: FrontSettings,
    /// The limit on a request to an S3 cold store.
    pub s3_request_timeout: Duration,
    /// How long the start-up self-test asks again of a cold store that does not answer.
    pub backend_check_wait: Duration,
    /// The cold store: the spec and the flags of a tier.
    pub cold: TierParts,
}

impl ColdSettings {
    /// The settings of the tables of a pass-through file.
    pub(super) fn new(
        server: ServerTable,
        cold: TierTable,
        policy: &[Policy],
        per_run: PerRun,
    ) -> Result<Self, ConfigError> {
        check_policies(policy, false)?;
        refuse_hot_keys(&cold)?;
        let list = [("endpoints", cold.endpoints.is_some())];
        let why = "this front has one cold store: give endpoint";
        refuse("cold_store", why, &list)?;
        if cold.skip_checks.is_some() {
            let why = "nothing can be skipped here: create-only has to hold at the cold store, completion of a multipart upload included";
            return Err(invalid("cold_store.skip_checks", why));
        }
        let parts = tier("cold_store", &cold)?;
        let shared = front_of(server, per_run)?;
        Ok(Self {
            front: shared.front,
            s3_request_timeout: shared.s3_request_timeout,
            backend_check_wait: shared.backend_check_wait,
            cold: parts,
        })
    }
}
