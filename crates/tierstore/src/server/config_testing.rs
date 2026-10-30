//! Fixtures of the tests of the configuration file, which `config_tests.rs`,
//! `config_print_tests.rs` and `launch_tests.rs` share.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;

use kbf_testkit::dir::TestDir;

use super::config::{DEFAULT_DRAIN_DEADLINE, DEFAULT_DRAIN_LB_WAIT, PerRun};

/// The knobs of a run that gives none of them.
pub(crate) const DEFAULTS: PerRun = PerRun {
    drain_lb_wait: DEFAULT_DRAIN_LB_WAIT,
    drain_deadline: DEFAULT_DRAIN_DEADLINE,
};

/// A storage node's file: two S3 tiers, every key of the key table given, a value that is not
/// the flag's default wherever the key has a default.
pub(crate) const GOLDEN: &str = r#"
[server]
listen                = "192.0.2.10:9300"
proxy_id              = "tierstore-1"
bucket                = "tierstore"
region                = "eu-west-1"
max_request_bytes     = 134217728
access_key_file       = "/etc/tierstore/access.key"
read_only_key_files   = ["/etc/tierstore/daemons.key", "/etc/tierstore/other.key"]
presign_key_files     = ["/etc/tierstore/pack-presign.key"]
gc_key_files          = ["/etc/tierstore/pack-gc.key"]
scoped_key_files      = ["/etc/tierstore/scoped.key"]
grant_key_files       = ["/etc/tierstore/grant.key"]
presign_max_expires   = "10m"
max_concurrent_puts   = 8
put_min_bytes_per_sec = 2048
put_rate_grace        = "45s"
body_idle_timeout     = "20s"
s3_request_timeout    = "2m"
backend_check_wait    = "90s"

[hot_store]
kind               = "s3"
bucket             = "tierstore-hot"
endpoint           = "http://192.0.2.10:9200"
region             = "us-east-1"
credentials_file   = "/etc/tierstore/hot.credentials"
budget             = "500GiB"
high_water         = 85
low_water          = 60
critical           = 97
copy_parallel      = 8
lease_ttl          = "40s"
reconcile_interval = "4s"
usage_stale_after  = "3m"
journal_linger     = "50ms"
journal_retention  = "12h"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://127.0.0.1:30900"
region           = "us-west-2"
credentials_file = "/etc/tierstore/cold.credentials"
skip_checks      = ["multipart-conditional=store-ignores-if-none-match-on-completion"]
"#;

/// The flags that say what `GOLDEN` says.
pub(crate) fn golden_flags() -> Vec<&'static str> {
    vec![
        "tierstore",
        "--listen=192.0.2.10:9300",
        "--proxy-id=tierstore-1",
        "--bucket=tierstore",
        "--region=eu-west-1",
        "--max-request-bytes=134217728",
        "--access-key-file=/etc/tierstore/access.key",
        "--read-only-key-file=/etc/tierstore/daemons.key",
        "--read-only-key-file=/etc/tierstore/other.key",
        "--presign-key-file=/etc/tierstore/pack-presign.key",
        "--gc-key-file=/etc/tierstore/pack-gc.key",
        "--scoped-key-file=/etc/tierstore/scoped.key",
        "--grant-key-file=/etc/tierstore/grant.key",
        "--presign-max-expires=10m",
        "--max-concurrent-puts=8",
        "--put-min-bytes-per-sec=2048",
        "--put-rate-grace=45s",
        "--body-idle-timeout=20s",
        "--s3-request-timeout=2m",
        "--backend-check-wait=90s",
        "--hot=s3:tierstore-hot",
        "--hot-endpoint=http://192.0.2.10:9200",
        "--hot-region=us-east-1",
        "--hot-credentials-file=/etc/tierstore/hot.credentials",
        "--hot-bytes=536870912000",
        "--high-water=85",
        "--low-water=60",
        "--critical=97",
        "--copy-parallel=8",
        "--lease-ttl=40s",
        "--reconcile-interval=4s",
        "--usage-stale-after=3m",
        "--journal-linger=50ms",
        "--journal-retention=12h",
        "--cold=s3:tierstore-cold",
        "--cold-endpoint=http://127.0.0.1:30900",
        "--cold-region=us-west-2",
        "--cold-credentials-file=/etc/tierstore/cold.credentials",
        "--skip-backend-check=multipart-conditional=store-ignores-if-none-match-on-completion",
    ]
}

/// The least a file can say: two tiers that are in memory.
pub(crate) const MINIMAL: &str = r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "p1"
bucket          = "tier-bucket"
access_key_file = "key"

[hot_store]
kind   = "memory"
budget = 1000

[cold_store]
kind = "memory"
"#;

/// The flags that say what `MINIMAL` says.
pub(crate) const MINIMAL_FLAGS: [&str; 8] = [
    "tierstore",
    "--listen=127.0.0.1:0",
    "--proxy-id=p1",
    "--bucket=tier-bucket",
    "--access-key-file=key",
    "--hot=memory",
    "--hot-bytes=1000",
    "--cold=memory",
];

/// Writes `text` as a key file in `dir` that only its owner can read.
pub(crate) fn write_private(dir: &TestDir, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    path
}
