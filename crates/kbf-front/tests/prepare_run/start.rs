//! `prepare_run`'s refusals at start: the configuration a [`RunPrep`] is built from is checked
//! once, before any run is prepared, and a wrong value is an error naming it.

use super::*;

#[tokio::test]
async fn a_configured_wall_over_four_hours_is_refused_at_start() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let storage: Arc<dyn Storage> = store;
    let mut over = config();
    over.run_wall = MAX_EXPIRY;
    over.slack = Duration::from_secs(1);
    let error = RunPrep::new(Backend::remote(storage), over).expect_err("refused at start");
    assert_eq!(
        error,
        ConfigError::Expiry(ExpiryError::OverCeiling {
            seconds: 14_401,
            ceiling: 14_400
        })
    );
}

#[tokio::test]
async fn a_wall_and_slack_of_nothing_are_refused_at_start() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let storage: Arc<dyn Storage> = store;
    let mut none = config();
    none.run_wall = Duration::ZERO;
    none.slack = Duration::ZERO;
    let error = RunPrep::new(Backend::remote(storage), none).expect_err("refused at start");
    assert_eq!(error, ConfigError::Expiry(ExpiryError::Zero));
}

#[tokio::test]
async fn an_http_listener_url_is_refused_at_start() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let cases = [
        ("http://127.0.0.1:8443", "is not https"),
        ("ftp://127.0.0.1:8443", "is not https"),
        ("127.0.0.1:8443", "is not of the form scheme://host[:port]"),
        ("https://", "has no host"),
        ("https://:8443", "has no host"),
        ("https://[]:8443", "has no host"),
        (
            "https://127.0.0.1:",
            "has a port that is not a number from 1 to 65535",
        ),
        (
            "https://127.0.0.1:0",
            "has a port that is not a number from 1 to 65535",
        ),
        (
            "https://127.0.0.1:65536",
            "has a port that is not a number from 1 to 65535",
        ),
        (
            "https://127.0.0.1:+80",
            "has a port that is not a number from 1 to 65535",
        ),
        (
            "https://127.0.0.1:84x3",
            "has a port that is not a number from 1 to 65535",
        ),
        ("https://[::1]x:8443", "has more than a host and a port"),
        (
            "https://127.0.0.1:8443/jobs",
            "has more than a host and a port",
        ),
        ("https://127.0.0.1:8443/", "has more than a host and a port"),
        (
            "https://user@127.0.0.1:8443",
            "has more than a host and a port",
        ),
        (
            "https://127.0.0.1:8443?x=1",
            "has more than a host and a port",
        ),
    ];
    for (url, reason) in cases {
        let mut bad = config();
        bad.read_url = url.to_owned();
        let storage: Arc<dyn Storage> = store.clone();
        let error = RunPrep::new(Backend::remote(storage), bad).expect_err("refused at start");
        assert_eq!(
            error,
            ConfigError::ReadUrl {
                url: url.to_owned(),
                why: reason
            }
        );
    }
    for url in [
        "https://127.0.0.1:8443",
        "https://[::1]:8443",
        "https://frontend.example",
    ] {
        assert_eq!(kbf_front::prepare_run::check_read_url(url), Ok(()), "{url}");
    }
}

/// Catches a plain-http read url accepted without the opt-in, and the opt-in widening more than
/// the scheme: with it `http://host[:port]` starts a preparer whose grants point there, and a
/// path, a missing host or another scheme is refused all the same.
#[tokio::test]
async fn a_plain_http_read_url_is_accepted_only_with_the_opt_in() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let storage: Arc<dyn Storage> = store.clone();
    let mut plain = config();
    plain.read_url = "http://127.0.0.1:8443".to_owned();
    let error = RunPrep::new(Backend::remote(storage.clone()), plain).expect_err("refused");
    assert_eq!(
        error,
        ConfigError::ReadUrl {
            url: "http://127.0.0.1:8443".to_owned(),
            why: "is not https"
        }
    );
    let mut allowed = config();
    allowed.read_url = "http://127.0.0.1:8443".to_owned();
    allowed.allow_plain_read_url = true;
    let prep = RunPrep::new(Backend::remote(storage.clone()), allowed).expect("accepted");
    let inputs = prep
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    assert_eq!(inputs.read_grant.url, "http://127.0.0.1:8443");
    for url in [
        "http://127.0.0.1:8443/jobs",
        "http://",
        "ftp://127.0.0.1:8443",
    ] {
        let mut bad = config();
        bad.read_url = url.to_owned();
        bad.allow_plain_read_url = true;
        let error = RunPrep::new(Backend::remote(storage.clone()), bad).expect_err("refused");
        assert!(
            matches!(error, ConfigError::ReadUrl { .. }),
            "{url}: {error:?}"
        );
    }
    assert!(kbf_front::prepare_run::check_read_url_allowing("http://127.0.0.1:8443", true).is_ok());
    assert!(
        kbf_front::prepare_run::check_read_url_allowing("http://127.0.0.1:8443", false).is_err()
    );
}

#[tokio::test]
async fn a_put_endpoint_that_is_not_http_or_https_host_is_refused_at_start() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let cases = [
        ("ftp://127.0.0.1:9000", "is not http or https"),
        ("http://:9000", "has no host"),
        (
            "http://127.0.0.1:99999",
            "has a port that is not a number from 1 to 65535",
        ),
        ("127.0.0.1:9000", "is not of the form scheme://host[:port]"),
        (
            "http://127.0.0.1:9000/tiers",
            "has more than a host and a port",
        ),
    ];
    for (url, reason) in cases {
        let mut bad = config();
        bad.put.endpoint = url.to_owned();
        let storage: Arc<dyn Storage> = store.clone();
        let error = RunPrep::new(Backend::remote(storage), bad).expect_err("refused at start");
        assert_eq!(
            error,
            ConfigError::PutEndpoint {
                url: url.to_owned(),
                why: reason
            }
        );
    }
    let mut tls = config();
    tls.put.endpoint = "https://127.0.0.1:9000".to_owned();
    prep_with(&store, tls);
}

#[tokio::test]
async fn an_empty_bucket_or_region_is_refused_at_start() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let mut no_bucket = config();
    no_bucket.put.bucket = String::new();
    let storage: Arc<dyn Storage> = store.clone();
    let error = RunPrep::new(Backend::remote(storage), no_bucket).expect_err("refused");
    assert_eq!(error, ConfigError::Empty("bucket"));
    let mut no_region = config();
    no_region.put.region = String::new();
    let storage: Arc<dyn Storage> = store;
    let error = RunPrep::new(Backend::remote(storage), no_region).expect_err("refused");
    assert_eq!(error, ConfigError::Empty("region"));
}
