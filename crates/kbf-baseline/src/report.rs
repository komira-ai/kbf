//! Builds turned into numbers: one [`BuildSummary`] per build and a [`Total`] over the
//! set, as JSON ([`Report`] serializes) and as a short table ([`render_table`]).
//!
//! Every time is in microseconds. A [`Timing`] is taken over the actions it applies
//! to: `action_us` over every action, `queue_us` over the actions executed remotely in
//! the build, `execution_us` over the actions executed (remotely or locally) in it. The
//! total's timings are taken over all the set's actions together, not averaged over
//! builds. The cache hit share is the remote cache hits over the actions that ran a
//! command or were answered for one (remote, cache hit, local, local cache).

use std::fmt::Write as _;

use serde::Serialize;

use crate::read::{Action, Build, Ran};

/// The report on a set of builds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub builds: Vec<BuildSummary>,
    pub total: Total,
}

/// One build's numbers.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BuildSummary {
    /// The log it was read from, as given.
    pub log: String,
    pub build_id: String,
    pub buck2_revision: Option<String>,
    pub success: Option<bool>,
    pub wall_us: Option<u64>,
    #[serde(flatten)]
    pub summary: Summary,
}

/// The set's numbers.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Total {
    pub builds: usize,
    /// Builds whose command ended unsuccessfully.
    pub failed_builds: usize,
    /// The sum of the builds' wall times (those that have one).
    pub wall_us: u64,
    #[serde(flatten)]
    pub summary: Summary,
}

/// What a set of actions shows.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Summary {
    pub actions: usize,
    pub ran: Counts,
    /// `None` when no action ran a command.
    pub cache_hit_share: Option<f64>,
    pub action_us: Timing,
    pub queue_us: Timing,
    pub execution_us: Timing,
    pub failures: Vec<Failure>,
}

/// Actions by where they ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub remote: usize,
    pub cache_hit: usize,
    pub local: usize,
    pub local_cache: usize,
    pub buck2: usize,
    pub unknown: usize,
}

impl Counts {
    fn add(&mut self, ran: Ran) {
        *match ran {
            Ran::Remote => &mut self.remote,
            Ran::CacheHit => &mut self.cache_hit,
            Ran::Local => &mut self.local,
            Ran::LocalCache => &mut self.local_cache,
            Ran::Buck2 => &mut self.buck2,
            Ran::Unknown => &mut self.unknown,
        } += 1;
    }

    /// Actions that ran a command or were answered for one.
    #[must_use]
    pub fn commands(&self) -> usize {
        self.remote + self.cache_hit + self.local + self.local_cache
    }
}

/// Nearest-rank percentiles and the maximum of some durations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Timing {
    pub count: usize,
    pub p50: Option<u64>,
    pub p90: Option<u64>,
    pub p99: Option<u64>,
    pub max: Option<u64>,
}

impl Timing {
    /// The timing of `values`, in any order.
    #[must_use]
    pub fn of(mut values: Vec<u64>) -> Self {
        values.sort_unstable();
        Self {
            count: values.len(),
            p50: nearest_rank(&values, 50),
            p90: nearest_rank(&values, 90),
            p99: nearest_rank(&values, 99),
            max: values.last().copied(),
        }
    }
}

/// The `p`th percentile of `sorted` by the nearest-rank method: the value at rank
/// `ceil(p / 100 * n)`, counting from 1 (so the 50th of 1..=10 is 5, and of 1..=4 is 2).
/// `None` for no values.
#[must_use]
pub fn nearest_rank(sorted: &[u64], p: usize) -> Option<u64> {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// A failed action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub build_id: String,
    pub target: String,
    pub category: String,
    pub identifier: String,
    pub ran: Ran,
    pub exit_code: Option<i64>,
    pub status: Option<String>,
}

impl Summary {
    /// The summary of `actions`, each with the id of the build it belongs to.
    #[must_use]
    pub fn of<'a>(actions: impl IntoIterator<Item = (&'a str, &'a Action)>) -> Self {
        let mut ran = Counts::default();
        let (mut action, mut queue, mut execution) = (Vec::new(), Vec::new(), Vec::new());
        let mut failures = Vec::new();
        for (build_id, a) in actions {
            ran.add(a.ran);
            action.push(a.action_us);
            queue.extend(a.queue_us);
            execution.extend(a.execution_us);
            if a.failed {
                failures.push(Failure {
                    build_id: build_id.to_owned(),
                    target: a.target.clone(),
                    category: a.category.clone(),
                    identifier: a.identifier.clone(),
                    ran: a.ran,
                    exit_code: a.exit_code,
                    status: a.status.clone(),
                });
            }
        }
        let commands = ran.commands();
        #[expect(clippy::cast_precision_loss, reason = "counts far below 2^52")]
        let cache_hit_share = if commands > 0 {
            Some(ran.cache_hit as f64 / commands as f64)
        } else {
            None
        };
        Self {
            actions: action.len(),
            ran,
            cache_hit_share,
            action_us: Timing::of(action),
            queue_us: Timing::of(queue),
            execution_us: Timing::of(execution),
            failures,
        }
    }
}

impl Report {
    /// The report on `builds`, each with the name of the log it was read from.
    #[must_use]
    pub fn of(builds: &[(String, Build)]) -> Self {
        let summaries = builds
            .iter()
            .map(|(log, b)| BuildSummary {
                log: log.clone(),
                build_id: b.build_id.clone(),
                buck2_revision: b.buck2_revision.clone(),
                success: b.success,
                wall_us: b.wall_us,
                summary: Summary::of(b.actions.iter().map(|a| (b.build_id.as_str(), a))),
            })
            .collect();
        let all = builds
            .iter()
            .flat_map(|(_, b)| b.actions.iter().map(|a| (b.build_id.as_str(), a)));
        let total = Total {
            builds: builds.len(),
            failed_builds: builds
                .iter()
                .filter(|(_, b)| b.success == Some(false))
                .count(),
            wall_us: builds.iter().filter_map(|(_, b)| b.wall_us).sum(),
            summary: Summary::of(all),
        };
        Self {
            builds: summaries,
            total,
        }
    }
}

/// Microseconds as milliseconds with one decimal, `-` for none.
fn ms(us: Option<u64>) -> String {
    match us {
        Some(us) => format!("{}.{}", us / 1000, us % 1000 / 100),
        None => "-".to_owned(),
    }
}

fn share(s: Option<f64>) -> String {
    match s {
        Some(s) => format!("{:.1}%", s * 100.0),
        None => "-".to_owned(),
    }
}

fn result(success: Option<bool>) -> &'static str {
    match success {
        Some(true) => "ok",
        Some(false) => "FAILED",
        None => "no end",
    }
}

fn counts_row(name: &str, res: &str, wall: Option<u64>, s: &Summary) -> String {
    let r = &s.ran;
    let other = r.local_cache + r.buck2 + r.unknown;
    format!(
        "{name:<32} {res:>6} {:>10} {:>6} {:>6} {:>6} {other:>6} {:>7} {:>6}\n",
        ms(wall),
        r.remote,
        r.cache_hit,
        r.local,
        share(s.cache_hit_share),
        s.failures.len(),
    )
}

/// The report as a short table: a row per build and the total, the total's timings in
/// milliseconds, and the failed actions. `other` is local cache hits, buck2's own
/// actions and unknown kinds together; the JSON keeps them apart.
#[must_use]
pub fn render_table(report: &Report) -> String {
    let mut out = format!(
        "{:<32} {:>6} {:>10} {:>6} {:>6} {:>6} {:>6} {:>7} {:>6}\n",
        "build", "result", "wall ms", "remote", "cache", "local", "other", "hits", "failed"
    );
    for b in &report.builds {
        let name = b.log.rsplit('/').next().unwrap_or(&b.log);
        out.push_str(&counts_row(name, result(b.success), b.wall_us, &b.summary));
    }
    let t = &report.total;
    let res = format!("{}/{}", t.builds - t.failed_builds, t.builds);
    out.push_str(&counts_row("total", &res, Some(t.wall_us), &t.summary));
    out.push('\n');
    let _ = writeln!(
        out,
        "{:<10} {:>6} {:>10} {:>10} {:>10} {:>10}",
        "ms", "count", "p50", "p90", "p99", "max"
    );
    let s = &t.summary;
    for (name, tm) in [
        ("action", &s.action_us),
        ("queue", &s.queue_us),
        ("execution", &s.execution_us),
    ] {
        let _ = writeln!(
            out,
            "{name:<10} {:>6} {:>10} {:>10} {:>10} {:>10}",
            tm.count,
            ms(tm.p50),
            ms(tm.p90),
            ms(tm.p99),
            ms(tm.max)
        );
    }
    for f in &s.failures {
        let exit = match f.exit_code {
            Some(c) => format!(", exit {c}"),
            None => String::new(),
        };
        let status = f.status.as_deref().unwrap_or("no command");
        let _ = writeln!(
            out,
            "FAILED {} ({}) {}: {status}{exit}, build {}",
            f.target, f.category, f.ran, f.build_id
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank() {
        // Catches: an off-by-one rank (an index of p*n/100 gives 6 for the 50th of
        // 1..=10), a rank rounded down (the 90th of 1..=4 would be 3, not 4), and a
        // percentile taken from unsorted values.
        let ten: Vec<u64> = (1..=10).collect();
        let pct = |v: &[u64], p| nearest_rank(v, p);
        assert_eq!(
            [pct(&ten, 50), pct(&ten, 90), pct(&ten, 99)],
            [Some(5), Some(9), Some(10)]
        );
        let hundred: Vec<u64> = (1..=100).collect();
        let got = [pct(&hundred, 50), pct(&hundred, 90), pct(&hundred, 99)];
        assert_eq!(got, [Some(50), Some(90), Some(99)]);
        assert_eq!(
            [pct(&[1, 2, 3, 4], 50), pct(&[1, 2, 3, 4], 90)],
            [Some(2), Some(4)]
        );
        assert_eq!(pct(&[7], 1), Some(7));
        assert_eq!(pct(&[], 50), None);
        let t = Timing::of(vec![30, 10, 20]);
        let expected = Timing {
            count: 3,
            p50: Some(20),
            p90: Some(30),
            p99: Some(30),
            max: Some(30),
        };
        assert_eq!(t, expected);
    }

    fn words(line: &str) -> String {
        line.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn action(target: &str, ran: Ran, failed: bool) -> Action {
        Action {
            target: target.to_owned(),
            category: "c".to_owned(),
            identifier: String::new(),
            ran,
            failed,
            action_us: 1500,
            queue_us: None,
            execution_us: None,
            exit_code: None,
            status: None,
        }
    }

    #[test]
    fn the_table_shows_what_the_json_has() {
        // Catches: local cache hits, buck2's own actions or unknown kinds left out of
        // `other`; a build with no end shown as ok; a failure that ran no command
        // printed without saying so; a set with no command given a share.
        let unfinished = Build {
            build_id: "b1".to_owned(),
            buck2_revision: None,
            success: None,
            wall_us: None,
            actions: vec![
                action("//:a", Ran::LocalCache, false),
                action("//:l", Ran::Local, false),
                action("//:b", Ran::Buck2, false),
                action("//:c", Ran::Unknown, true),
            ],
        };
        let mut failed = action("//:r", Ran::Remote, true);
        (failed.exit_code, failed.status) = (Some(1), Some("Failure".to_owned()));
        let finished = Build {
            build_id: "b0".to_owned(),
            buck2_revision: None,
            success: Some(true),
            wall_us: Some(999),
            actions: vec![action("//:h", Ran::CacheHit, false), failed],
        };
        let r = Report::of(&[
            ("dir/one.json-lines".to_owned(), unfinished),
            ("two.json-lines".to_owned(), finished),
        ]);
        let ran = r.total.summary.ran;
        let counts = [
            ran.remote,
            ran.cache_hit,
            ran.local,
            ran.local_cache,
            ran.buck2,
            ran.unknown,
        ];
        assert_eq!(counts, [1, 1, 1, 1, 1, 1]);
        assert_eq!(r.builds[0].summary.cache_hit_share, Some(0.0));
        let table = render_table(&r);
        let rows: Vec<String> = table.lines().map(words).collect();
        assert_eq!(rows[1], "one.json-lines no end - 0 0 1 3 0.0% 1");
        assert_eq!(rows[2], "two.json-lines ok 0.9 1 1 0 0 50.0% 1");
        assert_eq!(rows[3], "total 2/2 0.9 1 1 1 3 25.0% 2");
        assert_eq!(rows[6], "action 6 1.5 1.5 1.5 1.5");
        assert_eq!(rows[7], "queue 0 - - - -");
        assert_eq!(rows[9], "FAILED //:c (c) unknown: no command, build b1");
        assert_eq!(
            rows[10],
            "FAILED //:r (c) remote: Failure, exit 1, build b0"
        );

        // A failed build with no actions: no share, `-` for it, and its wall time.
        let empty = Build {
            build_id: "b2".to_owned(),
            buck2_revision: None,
            success: Some(false),
            wall_us: Some(2_000_100),
            actions: vec![],
        };
        let r = Report::of(&[("e.json-lines".to_owned(), empty)]);
        let s = &r.total.summary;
        assert_eq!((s.actions, s.cache_hit_share), (0, None));
        let rows: Vec<String> = render_table(&r).lines().map(words).collect();
        assert_eq!(rows[1], "e.json-lines FAILED 2000.1 0 0 0 0 - 0");
    }
}
