//! A reader for the Prometheus text format, where a series that is not there is `Absent`.

/// A measurement, or the lack of one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sample {
    /// A finite value.
    Value(f64),
    /// No such series, or one whose value is NaN or infinite. Never zero.
    Absent,
}

/// Why a scrape is refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("metrics line {line}: {why}")]
pub struct MetricsError {
    line: usize,
    why: &'static str,
}

struct Series {
    name: String,
    labels: Vec<(String, String)>,
    value: f64,
}

/// One scrape of a `/metrics` endpoint.
pub struct Metrics {
    series: Vec<Series>,
}

impl Metrics {
    /// Parse exposition text. A line that is neither blank, a comment nor a well-formed
    /// sample refuses the whole scrape: a scrape that cannot be read is not evidence.
    pub fn parse(text: &str) -> Result<Self, MetricsError> {
        let mut series = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fail = |why| MetricsError { line: i + 1, why };
            let name_end = line.find(['{', ' ', '\t']).ok_or(fail("no value"))?;
            let (name, mut rest) = line.split_at(name_end);
            if name.is_empty() {
                return Err(fail("no metric name"));
            }
            let mut labels = Vec::new();
            if let Some(body) = rest.strip_prefix('{') {
                rest = labels_of(body, &mut labels).ok_or(fail("bad labels"))?;
            }
            let value = rest.split_whitespace().next().ok_or(fail("no value"))?;
            let value = value.parse::<f64>().map_err(|_| fail("bad value"))?;
            series.push(Series {
                name: name.to_owned(),
                labels,
                value,
            });
        }
        Ok(Self { series })
    }

    /// The sum of every series named `name`; `Absent` when there is none or any is not finite.
    pub fn sum(&self, name: &str) -> Sample {
        self.sum_matching(name, |_| true)
    }

    /// The sum of the series named `name` that carry the label `key="value"`.
    pub fn sum_where(&self, name: &str, key: &str, value: &str) -> Sample {
        self.sum_matching(name, |s| {
            s.labels.iter().any(|(k, v)| k == key && v == value)
        })
    }

    fn sum_matching(&self, name: &str, keep: impl Fn(&Series) -> bool) -> Sample {
        let mut total = 0.0;
        let mut seen = false;
        for s in self.series.iter().filter(|s| s.name == name && keep(s)) {
            if !s.value.is_finite() {
                return Sample::Absent;
            }
            total += s.value;
            seen = true;
        }
        if seen {
            Sample::Value(total)
        } else {
            Sample::Absent
        }
    }
}

/// Parse `k="v",k2="v2"}` into `labels` and return what follows the closing brace.
fn labels_of<'a>(mut s: &'a str, labels: &mut Vec<(String, String)>) -> Option<&'a str> {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix('}') {
            return Some(rest);
        }
        let eq = s.find("=\"")?;
        let key = s[..eq].trim().to_owned();
        let mut value = String::new();
        let mut chars = s[eq + 2..].char_indices();
        let end = loop {
            match chars.next()? {
                (i, '"') => break eq + 2 + i + 1,
                (_, '\\') => value.push(match chars.next()?.1 {
                    'n' => '\n',
                    c => c,
                }),
                (_, c) => value.push(c),
            }
        };
        labels.push((key, value));
        s = s[end..].trim_start();
        s = s.strip_prefix(',').unwrap_or(s);
    }
}
