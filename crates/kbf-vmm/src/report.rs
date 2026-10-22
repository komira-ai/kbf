//! The JSON object every probe prints, and the statistics `probe-bench` reports.

use serde::Serialize;
use serde_json::{Value, json};

/// How a probe ended. `Ok` means the probe ran and `details` holds the answer, whatever it
/// is (a clone that does not boot is an answer). `NeedsManual` means the probe could not
/// run, or could not run all of it, without something this binary cannot create; `reason`
/// says exactly what. `Error` means the probe broke on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Ok,
    NeedsManual,
    Error,
}

/// One probe's result: printed as one JSON object on one line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub probe: String,
    pub status: Status,
    /// What was found, in one sentence (status `ok`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// What is missing or what broke (status `needs-manual` or `error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The measurements and observations behind the verdict.
    pub details: Value,
    /// The host the probe ran on: macOS version and build, architecture, this binary.
    pub host: Value,
}

impl Report {
    /// A finished probe.
    #[must_use]
    pub fn ok(probe: &str, summary: impl Into<String>, details: Value) -> Self {
        Self {
            probe: probe.to_owned(),
            status: Status::Ok,
            summary: Some(summary.into()),
            reason: None,
            details,
            host: Value::Null,
        }
    }

    /// A probe that needs a manual step; `details` keeps whatever part could run.
    #[must_use]
    pub fn needs_manual(probe: &str, reason: impl Into<String>, details: Value) -> Self {
        Self {
            probe: probe.to_owned(),
            status: Status::NeedsManual,
            summary: None,
            reason: Some(reason.into()),
            details,
            host: Value::Null,
        }
    }

    /// A probe that broke.
    #[must_use]
    pub fn error(probe: &str, reason: impl Into<String>) -> Self {
        Self {
            probe: probe.to_owned(),
            status: Status::Error,
            summary: None,
            reason: Some(reason.into()),
            details: json!({}),
            host: Value::Null,
        }
    }

    /// The same report with the host facts filled in.
    #[must_use]
    pub fn on_host(mut self, host: Value) -> Self {
        self.host = host;
        self
    }

    /// The one-line JSON text.
    #[must_use]
    pub fn to_line(&self) -> String {
        // A `Value` tree with string keys always serializes.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The median and maximum of a set of millisecond measurements (the sample is small: no
/// percentile above the median is claimed).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub n: usize,
    pub median_ms: f64,
    pub max_ms: f64,
}

/// Median (mean of the two middle values for an even count) and maximum of `samples`;
/// `None` for no samples.
#[must_use]
pub fn summarize(samples: &[f64]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let median_ms = if n % 2 == 1 {
        v[n / 2]
    } else {
        f64::midpoint(v[n / 2 - 1], v[n / 2])
    };
    Some(Summary {
        n,
        median_ms,
        max_ms: v[n - 1],
    })
}

/// [`summarize`] as JSON: the summary, or `null` for no samples.
#[must_use]
pub fn summary_json(samples: &[f64]) -> Value {
    summarize(samples).map_or(Value::Null, |s| json!(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a report whose JSON drops a field a reader of the result relies on, or
    /// prints the wrong status spelling: each kind has its own required keys.
    #[test]
    fn each_kind_of_report_prints_its_keys() {
        let ok = serde_json::to_value(Report::ok("g1", "fine", json!({"n": 1}))).expect("json");
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["summary"], "fine");
        assert!(ok.get("reason").is_none());
        assert_eq!(ok["details"]["n"], 1);
        let nm = serde_json::to_value(Report::needs_manual("g2", "why", json!({}))).expect("json");
        assert_eq!(nm["status"], "needs-manual");
        assert_eq!(nm["reason"], "why");
        assert!(nm.get("summary").is_none());
        let err = serde_json::to_value(Report::error("g3", "broke")).expect("json");
        assert_eq!(err["status"], "error");
        assert_eq!(err["probe"], "g3");
        assert_eq!(err["details"], json!({}));
    }

    /// Catches host facts that are not attached, and output that is not one line.
    #[test]
    fn the_line_is_one_object_with_the_host() {
        let line = Report::ok("g1", "s", json!({"a": "x\ny"}))
            .on_host(json!({"macos": "27"}))
            .to_line();
        assert!(!line.contains('\n'));
        let v: Value = serde_json::from_str(&line).expect("one JSON object");
        assert_eq!(v["host"]["macos"], "27");
    }

    /// Catches a median taken from the unsorted input, an even count that takes one
    /// middle value, a maximum taken from the end of the input, and a panic on no input.
    #[test]
    fn summary_has_median_and_max() {
        assert!(summarize(&[]).is_none());
        let odd = summarize(&[9.0, 1.0, 5.0]).expect("odd");
        assert_eq!((odd.n, odd.median_ms, odd.max_ms), (3, 5.0, 9.0));
        let even = summarize(&[8.0, 2.0, 4.0, 100.0, 6.0, 10.0]).expect("even");
        assert_eq!((even.n, even.median_ms, even.max_ms), (6, 7.0, 100.0));
        let one = summarize(&[3.5]).expect("one");
        assert_eq!((one.median_ms, one.max_ms), (3.5, 3.5));
    }

    /// Catches an empty sample printed as a zero summary instead of null.
    #[test]
    fn summary_json_is_null_without_samples() {
        assert_eq!(summary_json(&[]), Value::Null);
        assert_eq!(summary_json(&[2.0, 4.0])["median_ms"], 3.0);
    }
}
