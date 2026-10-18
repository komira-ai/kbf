//! The gate helper and the metrics reader: absent is red, boundaries are exact.

use kbf_gate::{GateResult, Limit, Metrics, Sample, Verdict, judge};

const SCRAPE: &str = r#"
# HELP kbf_cache_lost_blobs blobs lost
# TYPE kbf_cache_lost_blobs gauge
kbf_cache_lost_blobs 0
kbf_frontend_relay_untraced{frontend="a"} 0
kbf_frontend_relay_untraced{frontend="b"} 2
kbf_execute_requests_total{state="ok",note="a \"quoted\" } brace"} 40 1700000000
kbf_execute_requests_total{state="err"} 2
kbf_tier_odd NaN
kbf_tier_big +Inf
kbf_trailing{a="1",} 5
"#;

fn scrape() -> Metrics {
    Metrics::parse(SCRAPE).unwrap()
}

/// Catches the reading of a missing series as zero: the series is not in the scrape, so the
/// sample is Absent, and a "must be zero" gate on it is red.
#[test]
fn an_absent_series_is_absent_and_red_under_every_limit() {
    let m = scrape();
    let sample = m.sum("kbf_cache_blob_losses_total");
    assert_eq!(sample, Sample::Absent);
    for limit in [
        Limit::AtMost(0.0),
        Limit::AtLeast(0.0),
        Limit::Band {
            warn: 1.0,
            red: 2.0,
        },
    ] {
        assert_eq!(judge(sample, limit), Verdict::Red, "{limit:?}");
    }
    let g = GateResult::decide("lost-blob-losses", sample, Limit::AtMost(0.0), 1, 2);
    assert_eq!((g.measured, g.verdict), (None, Verdict::Red));
}

#[test]
fn a_present_zero_is_green() {
    assert_eq!(scrape().sum("kbf_cache_lost_blobs"), Sample::Value(0.0));
    assert_eq!(
        judge(Sample::Value(0.0), Limit::AtMost(0.0)),
        Verdict::Green
    );
}

/// Equal passes, one step above fails, for both directions and both band edges.
#[test]
fn thresholds_are_exact() {
    let up = |v| judge(Sample::Value(v), Limit::AtMost(5.0));
    assert_eq!((up(5.0), up(5.000_001)), (Verdict::Green, Verdict::Red));
    let down = |v| judge(Sample::Value(v), Limit::AtLeast(5.0));
    assert_eq!((down(5.0), down(4.999_999)), (Verdict::Green, Verdict::Red));
    let band = |v| {
        judge(
            Sample::Value(v),
            Limit::Band {
                warn: 1.5,
                red: 3.0,
            },
        )
    };
    assert_eq!(band(1.5), Verdict::Green);
    assert_eq!(band(1.51), Verdict::Warn);
    assert_eq!(band(3.0), Verdict::Warn);
    assert_eq!(band(3.01), Verdict::Red);
}

#[test]
fn series_sum_and_filter_by_label() {
    let m = scrape();
    assert_eq!(m.sum("kbf_frontend_relay_untraced"), Sample::Value(2.0));
    assert_eq!(
        m.sum_where("kbf_frontend_relay_untraced", "frontend", "a"),
        Sample::Value(0.0)
    );
    assert_eq!(
        m.sum_where("kbf_frontend_relay_untraced", "frontend", "b"),
        Sample::Value(2.0)
    );
    assert_eq!(
        m.sum_where("kbf_frontend_relay_untraced", "frontend", "c"),
        Sample::Absent
    );
    assert_eq!(m.sum("kbf_execute_requests_total"), Sample::Value(42.0));
    assert_eq!(
        m.sum_where("kbf_execute_requests_total", "note", "a \"quoted\" } brace"),
        Sample::Value(40.0)
    );
    assert_eq!(m.sum_where("kbf_trailing", "a", "1"), Sample::Value(5.0));
}

/// A NaN or infinite value is not a measurement: Absent, hence red, never a pass by a failed
/// comparison and never a write error.
#[test]
fn a_non_finite_value_is_absent() {
    let m = scrape();
    assert_eq!(m.sum("kbf_tier_odd"), Sample::Absent);
    assert_eq!(m.sum("kbf_tier_big"), Sample::Absent);
}

#[test]
fn a_scrape_that_cannot_be_read_is_refused() {
    for (text, why) in [
        ("name_only", "no value"),
        ("{a=\"1\"} 3", "no metric name"),
        ("m{a=\"1\" 3", "bad labels"),
        ("m{a=1} 3", "bad labels"),
        ("m{a=\"1} 3", "bad labels"),
        ("m{a=\"x\\", "bad labels"),
        ("m{a=\"1\"}", "no value"),
        ("ok 1\nm twelve", "bad value"),
    ] {
        let e = Metrics::parse(text)
            .err()
            .unwrap_or_else(|| panic!("{text:?} parsed"));
        assert!(e.to_string().ends_with(why), "{text:?}: {e}");
    }
    assert_eq!(
        Metrics::parse("ok 1\nm twelve").err().unwrap().to_string(),
        "metrics line 2: bad value"
    );
    assert_eq!(
        Metrics::parse("m 1\n").unwrap().sum("m"),
        Sample::Value(1.0)
    );
    assert_eq!(Metrics::parse("m\t2").unwrap().sum("m"), Sample::Value(2.0));
    assert_eq!(
        Metrics::parse("m{a=\"x\\ny\"} 2")
            .unwrap()
            .sum_where("m", "a", "x\ny"),
        Sample::Value(2.0)
    );
}

/// A NaN handed in as a value is recorded as not measured, and is red.
#[test]
fn a_nan_sample_is_recorded_as_not_measured() {
    let g = GateResult::decide("g", Sample::Value(f64::NAN), Limit::AtMost(0.0), 1, 2);
    assert_eq!((g.measured, g.verdict), (None, Verdict::Red));
    assert!(
        !Limit::Band {
            warn: 1.0,
            red: f64::INFINITY
        }
        .is_finite()
    );
    assert!(
        Limit::Band {
            warn: 1.0,
            red: 2.0
        }
        .is_finite()
    );
    assert!(!Limit::AtLeast(f64::NAN).is_finite());
}
