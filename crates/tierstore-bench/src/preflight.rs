//! `preflight`: read-only checks before anything is written, and the before-snapshot
//! [`crate::teardown`] compares against.
//!
//! It reads the ops file (`ops.json` of the result directory, see [`ops`]) and the run
//! configuration, changes nothing on a store or a machine, and answers, one [`Check`] each:
//! - `pinned-binary-digest`: the pinned tierstore binary has the pinned SHA-256;
//! - `plan-flags-in-help`: `--help` of that binary lists every flag the plan uses;
//! - `cold-only-mode`: whether the binary can run with a cold tier only (a finding, not a
//!   requirement: it passes either way and says which);
//! - `live-binary-read`, `live-unit-read`: the live tierstore's binary digest, its unit's
//!   properties and its command line's flag names (flag parity), read-only;
//! - `store-version:<role>` and, for the cold store, `cold-heal-state`: the store's version and
//!   whether its drives are healing, from `mc admin info` and the operator's status file;
//! - `node-facts`: cores, memory, system, kernel and link speed, by role;
//! - `load-gate`: the machine's load against the gate's limits;
//! - `paper-scan`: the paper's open places against the cell map (when the ops file names them);
//! - `test-instance-isolated`, `test-instance-absent`: the test instance's unit, port and
//!   directories are its own, and none of it exists yet.
//!
//! Every check runs whatever the others found, so one run lists every problem. The report and
//! the snapshot are written to the result directory (`preflight.json`, `before.json`) and the
//! checks are printed; the verb ends in an error if any check failed. The snapshot holds hashes
//! only; the report holds role names, versions, counts and flag names, no host, address, bucket
//! or key.

pub mod facts;
pub mod help;
pub mod host;
pub mod live;
pub mod mc;
pub mod ops;
pub mod paper;
pub mod trusted;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use self::host::{Cmd, Host, SystemHost, read_text, run_ok};
use self::mc::Mc;
use self::ops::OpsFile;
use crate::config::RunConfig;
use crate::error::BenchError;
use crate::result::{Check, Hardware, Provenance};
use crate::role::Role;

/// The report file of the result directory.
pub const REPORT_FILE: &str = "preflight.json";

/// The snapshot file of the result directory: what `verify-teardown --before` reads.
pub const SNAPSHOT_FILE: &str = "before.json";

/// What preflight recorded, as name and value (hashes and counts, never contents).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The facts, by name.
    pub facts: BTreeMap<String, String>,
}

/// The outcome of a preflight.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightReport {
    /// One entry per check.
    pub checks: Vec<Check>,
    /// The before-snapshot, written to the result directory for `verify-teardown`.
    pub snapshot: Snapshot,
    /// SHA-256 of the pinned binary, empty if it could not be read.
    pub binary_digest: String,
    /// Versions and states by name: `tierstore`, `<role>-store`, `cold-heal`.
    pub tool_versions: BTreeMap<String, String>,
    /// The machine's facts, for each role the ops file says it plays.
    pub hardware: BTreeMap<Role, Hardware>,
}

impl PreflightReport {
    /// Whether every check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// The provenance block of a result, with the facts of this preflight. The cold store's
    /// heal state is among the `tool_versions` (`cold-heal`), so a result that carries this
    /// block carries the state its numbers were taken in.
    #[must_use]
    pub fn provenance(&self, run_id: &str, git_sha: &str) -> Provenance {
        Provenance {
            run_id: run_id.to_owned(),
            git_sha: git_sha.to_owned(),
            binary_digest: self.binary_digest.clone(),
            tool_versions: self.tool_versions.clone(),
            hardware: self.hardware.clone(),
            attempt: 1,
            open_loop: true,
            arms_filter: None,
            cold_server: None,
        }
    }
}

/// A failure in words for a check's detail: no path, host, bucket or key. A file that cannot be
/// read is named by the operating system's error class, a store configuration that is refused
/// by that fact alone (the refusal text quotes the endpoint and the bucket).
#[must_use]
pub fn why(error: &BenchError) -> String {
    match error {
        BenchError::Io { source, .. } => format!("a file cannot be read: {:?}", source.kind()),
        BenchError::Store(_) => "a store's endpoint or bucket name is not accepted".to_owned(),
        other => other.to_string(),
    }
}

/// A check from what was found: `Ok` is a pass with the text as its detail, `Err` a fail.
#[must_use]
pub fn check(name: &str, found: Result<String, String>) -> Check {
    let (passed, detail) = match found {
        Ok(detail) => (true, detail),
        Err(detail) => (false, detail),
    };
    Check {
        name: name.to_owned(),
        passed,
        detail,
    }
}

/// Writes `value` as indented JSON to `path`, creating the directory above it.
///
/// # Errors
/// [`BenchError::Io`] if the directory or the file cannot be written.
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), BenchError> {
    let mut text = serde_json::to_string_pretty(value).expect("a report serialises");
    text.push('\n');
    let io = |source| BenchError::Io {
        path: path.to_owned(),
        source,
    };
    // A path with no parent is the empty path's neighbour: `create_dir_all("")` is a no-op.
    fs::create_dir_all(path.parent().unwrap_or(Path::new(""))).map_err(io)?;
    fs::write(path, text).map_err(io)
}

/// Prints each check as `PASS name: detail` or `FAIL name: detail`.
pub fn print_checks(checks: &[Check]) {
    for c in checks {
        let verdict = if c.passed { "PASS" } else { "FAIL" };
        println!("{verdict} {}: {}", c.name, c.detail);
    }
}

/// The error a verb ends in when checks failed, or `Ok` if none did.
///
/// # Errors
/// [`BenchError::Ops`] naming the verb and the checks that failed.
pub fn all_passed(verb: &str, checks: &[Check]) -> Result<(), BenchError> {
    let failed: Vec<&str> = checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name.as_str())
        .collect();
    if failed.is_empty() {
        return Ok(());
    }
    Err(BenchError::Ops(format!(
        "{verb}: {} of {} checks failed: {}",
        failed.len(),
        checks.len(),
        failed.join(", ")
    )))
}

/// SHA-256 of the file at `path`, or the reason it cannot be read.
pub(crate) fn digest_of(host: &dyn Host, path: &Path) -> Result<String, String> {
    host.read_file(path)
        .map(|bytes| live::sha256_hex(&bytes))
        .map_err(|e| format!("cannot read the binary: {:?}", e.kind()))
}

/// `Ok` if `found` is the pinned digest.
fn is_pinned(found: &str, pinned: &str) -> Result<String, String> {
    if found.eq_ignore_ascii_case(pinned) {
        Ok(format!("sha256 {found}"))
    } else {
        Err(format!("sha256 {found} is not the pinned digest"))
    }
}

/// The percent of the cold store's heal done, from the operator's status file. `None` if the
/// ops file names no such file.
pub(crate) fn heal_percent(host: &dyn Host, ops: &OpsFile) -> Result<Option<f64>, String> {
    #[derive(Deserialize)]
    struct Status {
        percent_done: f64,
    }
    let Some(path) = &ops.cold_heal_status_file else {
        return Ok(None);
    };
    let text = host::read_status_text(host, path).map_err(|e| why(&e))?;
    let status: Status =
        serde_json::from_str(&text).map_err(|e| format!("heal status file: {e}"))?;
    Ok(Some(status.percent_done))
}

/// Whether the cold store is healing: a drive that says it heals, or a status file under 100%.
/// One rule for the preflight's words ([`heal_state`]) and for the readings the run takes before
/// every pass ([`crate::coldstate`]).
#[must_use]
pub fn is_healing(info: Option<&serde_json::Value>, percent_done: Option<f64>) -> bool {
    let drives = info.is_some_and(mc::any_drive_healing);
    let file = percent_done.is_some_and(|p| p < 100.0);
    drives || file
}

/// The heal state in words: `none`, `healing N% done` or `healing, progress unknown`.
#[must_use]
pub fn heal_state(info: Option<&serde_json::Value>, percent_done: Option<f64>) -> String {
    heal_words(is_healing(info, percent_done), percent_done)
}

fn heal_words(healing: bool, percent_done: Option<f64>) -> String {
    match (healing, percent_done) {
        (true, Some(p)) => format!("healing {p:.0}% done"),
        (true, None) => "healing, progress unknown".to_owned(),
        (false, _) => "none".to_owned(),
    }
}

/// How far in the future the stamp of a status file may be, milliseconds: the writer's clock
/// and this one are not the same clock.
const FUTURE_SKEW_MS: u64 = 5_000;

/// The cold store's status file in file mode: what the writer on the node that can ask the store
/// recorded, stamped with its own clock when it asked.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct HealFile {
    /// When the writer read the store, milliseconds since the Unix epoch, by its clock.
    pub read_unix_ms: u64,
    /// Whether the writer saw the store healing.
    pub healing: bool,
    /// How far the heal is, percent, if the writer knows.
    pub percent_done: Option<f64>,
    /// The drives the writer saw, named as in a result.
    pub drives: Vec<crate::result::DriveState>,
    /// The store's version, if the writer recorded it.
    pub version: Option<String>,
}

impl HealFile {
    /// Healing if the file's flag, a drive or the percent says so.
    pub(crate) fn is_healing(&self) -> bool {
        self.healing
            || self.drives.iter().any(|d| d.healing)
            || self.percent_done.is_some_and(|p| p < 100.0)
    }

    /// The heal state in words, as [`heal_state`].
    pub(crate) fn state_words(&self) -> String {
        heal_words(self.is_healing(), self.percent_done)
    }
}

/// The status file `path` (see [`OpsFile::cold_file_mode`]), read in file mode and refused unless its own stamp is at
/// most `max_age` before `now_ms` and at most five seconds after it.
pub(crate) fn read_heal_file(
    host: &dyn Host,
    path: &Path,
    now_ms: u64,
    max_age: Duration,
) -> Result<HealFile, String> {
    let text = host::read_status_text(host, path).map_err(|e| why(&e))?;
    let file: HealFile =
        serde_json::from_str(&text).map_err(|e| format!("heal status file: {e}"))?;
    if file.read_unix_ms > now_ms.saturating_add(FUTURE_SKEW_MS) {
        return Err(format!(
            "heal status file is stamped {} s in the future",
            (file.read_unix_ms - now_ms) / 1000
        ));
    }
    let limit = u64::try_from(max_age.as_millis()).unwrap_or(u64::MAX);
    let age = now_ms.saturating_sub(file.read_unix_ms);
    if age > limit {
        return Err(format!(
            "heal status file is {} s old, over the limit of {} s",
            age / 1000,
            max_age.as_secs()
        ));
    }
    Ok(file)
}

/// What of the test instance exists: its unit, its port and its directories, one text each.
///
/// # Errors
/// [`BenchError::Ops`] if the unit's state cannot be read.
pub fn instance_leftovers(
    host: &dyn Host,
    instance: &ops::TestInstance,
) -> Result<Vec<String>, BenchError> {
    let mut found = Vec::new();
    if !live::unit_absent(host, &instance.unit)? {
        found.push("the unit is loaded".to_owned());
    }
    if host.port_open(instance.port) {
        found.push("the port accepts connections".to_owned());
    }
    found.extend(
        instance
            .dirs
            .iter()
            .filter(|dir| host.exists(dir))
            .map(|_| "a directory exists".to_owned()),
    );
    Ok(found)
}

/// The checks that ask a store: its version, and for the cold store its heal state.
///
/// The cold role with no admin entry and a status file (file mode) is answered by the file
/// alone, under its freshness rule, and `mc` is not run for it.
fn store_checks(
    host: &dyn Host,
    config: &RunConfig,
    ops: &OpsFile,
    now_ms: u64,
    versions: &mut BTreeMap<String, String>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut cold_info = None;
    let path = ops
        .cold_file_mode()
        .filter(|_| config.stores.contains_key(&Role::Cold));
    let file = path.map(|path| read_heal_file(host, path, now_ms, config.cold_state_max_age));
    for role in [Role::Cold, Role::Hot] {
        let Some(spec) = config.stores.get(&role) else {
            continue;
        };
        if let (Role::Cold, Some(file)) = (role, &file) {
            let version = file.clone().map(|f| {
                f.version
                    .inspect(|v| {
                        versions.insert("cold-store".to_owned(), v.clone());
                    })
                    .unwrap_or_else(|| "version unknown (file mode)".to_owned())
            });
            checks.push(check("store-version:cold", version));
            continue;
        }
        let info = Mc::for_role(host, ops, role, spec, &config.result_dir)
            .and_then(|mc| mc.info())
            .map_err(|e| why(&e));
        let version = info.as_ref().map_err(Clone::clone).and_then(|value| {
            mc::server_version(value).ok_or_else(|| "the admin info has no version".to_owned())
        });
        if let Ok(v) = &version {
            versions.insert(format!("{role}-store"), v.clone());
        }
        checks.push(check(&format!("store-version:{role}"), version));
        if role == Role::Cold {
            cold_info = Some(info);
        }
    }
    if let Some(file) = file {
        let found = file.map(|f| f.state_words());
        if let Ok(state) = &found {
            versions.insert("cold-heal".to_owned(), state.clone());
        }
        checks.push(check("cold-heal-state", found));
    } else if let Some(info) = cold_info {
        let percent = heal_percent(host, ops);
        let info = info.ok();
        let known = info.is_some() || matches!(percent, Ok(Some(_)));
        let state = heal_state(info.as_ref(), percent.clone().ok().flatten());
        versions.insert("cold-heal".to_owned(), state.clone());
        let found = percent.and_then(|_| {
            if known {
                Ok(state)
            } else {
                Err("neither the admin info nor a status file gives the heal state".to_owned())
            }
        });
        checks.push(check("cold-heal-state", found));
    }
    checks
}

/// The checks of the pinned binary: its digest, its flags, and cold-only mode.
fn binary_checks(host: &dyn Host, ops: &OpsFile, snapshot: &mut Snapshot) -> (Vec<Check>, String) {
    let pinned = digest_of(host, &ops.tierstore.binary);
    let digest = pinned.clone().unwrap_or_default();
    if pinned.is_ok() {
        snapshot
            .facts
            .insert("pinned_binary_sha256".to_owned(), digest.clone());
    }
    let help = run_ok(
        host,
        "tierstore --help",
        &Cmd::new(&ops.tierstore.binary, &["--help"]),
    )
    .map(|text| help::parse_help(&text))
    .map_err(|e| why(&e));
    let flags = help.clone().and_then(|h| {
        let missing = h.missing(&ops.tierstore.plan_flags);
        if missing.is_empty() {
            Ok(format!(
                "all {} plan flags are listed",
                ops.tierstore.plan_flags.len()
            ))
        } else {
            Err(format!("not listed: {}", missing.join(" ")))
        }
    });
    let cold_only = help.map(|h| {
        if h.cold_only_possible() {
            "possible: --hot is optional".to_owned()
        } else {
            "not possible: --hot is required".to_owned()
        }
    });
    let checks = vec![
        check(
            "pinned-binary-digest",
            pinned.and_then(|d| is_pinned(&d, &ops.tierstore.sha256)),
        ),
        check("plan-flags-in-help", flags),
        check("cold-only-mode", cold_only),
    ];
    (checks, digest)
}

/// The checks of the live tierstore on this node, and the snapshot facts they give.
fn live_checks(host: &dyn Host, ops: &OpsFile, snapshot: &mut Snapshot) -> Vec<Check> {
    let binary = digest_of(host, &ops.live.binary);
    if let Ok(digest) = &binary {
        snapshot
            .facts
            .insert("live_binary_sha256".to_owned(), digest.clone());
    }
    let unit = live::read_unit(host, &ops.live.unit).map_err(|e| why(&e));
    if let Ok(view) = &unit {
        snapshot
            .facts
            .insert("live_unit_show_sha256".to_owned(), view.show_sha256.clone());
        snapshot
            .facts
            .insert("live_flags_sha256".to_owned(), view.argv_sha256.clone());
    }
    let unit_found = unit.map(|view| {
        let unset: Vec<&str> = ops
            .tierstore
            .plan_flags
            .iter()
            .filter(|flag| !view.flags.contains(flag.as_str()))
            .map(String::as_str)
            .collect();
        format!(
            "{} flags set ({}); plan flags not set live: {}",
            view.flags.len(),
            view.flags
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            unset.join(" ")
        )
    });
    vec![
        check("live-binary-read", binary.map(|d| format!("sha256 {d}"))),
        check("live-unit-read", unit_found),
    ]
}

/// The checks of this machine: facts by role, the load gate, and the test instance.
fn node_checks(
    host: &dyn Host,
    config: &RunConfig,
    ops: &OpsFile,
    hardware: &mut BTreeMap<Role, Hardware>,
) -> Vec<Check> {
    let facts = if ops.node.roles.is_empty() {
        Err("the ops file names no role for this machine".to_owned())
    } else {
        facts::read_hardware(host, ops.node.link_interface.as_deref()).map_err(|e| why(&e))
    };
    if let Ok(found) = &facts {
        hardware.extend(ops.node.roles.iter().map(|role| (*role, found.clone())));
    }
    let facts_found = facts.map(|h| {
        format!(
            "{} cores, {} bytes of memory, {}, kernel {}, link {:?} Mbit/s",
            h.cpu_cores, h.memory_bytes, h.os, h.kernel, h.link_mbps
        )
    });
    let gate = host.load_sample().map_err(|e| why(&e)).and_then(|s| {
        config
            .gate
            .check(&s)
            .map(|()| {
                format!(
                    "load {:.2}, cpu pressure {:.1}, io pressure {:.1}, disk {:.1}%",
                    s.loadavg_1m, s.psi_cpu_avg10, s.psi_io_avg10, s.disk_util_pct
                )
            })
            .map_err(|trip| trip.to_string())
    });
    let problems = ops.isolation_problems();
    let isolated = if problems.is_empty() {
        Ok("own unit, own port, own directories".to_owned())
    } else {
        Err(problems.join("; "))
    };
    let absent = instance_leftovers(host, &ops.test_instance)
        .map_err(|e| why(&e))
        .and_then(|left| {
            if left.is_empty() {
                Ok("no unit, nothing on the port, no directory".to_owned())
            } else {
                Err(left.join("; "))
            }
        });
    vec![
        check("node-facts", facts_found),
        check("load-gate", gate),
        check("test-instance-isolated", isolated),
        check("test-instance-absent", absent),
    ]
}

/// The check of the paper against its map, if the ops file names both.
fn paper_checks(host: &dyn Host, ops: &OpsFile) -> Vec<Check> {
    let Some(files) = &ops.paper else {
        return Vec::new();
    };
    let found = read_text(host, &files.page)
        .and_then(|page| read_text(host, &files.map).map(|map| (page, map)))
        .map_err(|e| why(&e))
        .and_then(|(page, map)| paper::scan(&page, &map).map_err(|e| why(&e)))
        .and_then(|scan| {
            if scan.agrees() {
                Ok(format!("{} open places, every one in the map", scan.marked))
            } else {
                Err(format!(
                    "lines open in the paper but not in the map: {:?}; map lines not open in the paper: {:?}",
                    scan.unmapped, scan.stale
                ))
            }
        });
    vec![check("paper-scan", found)]
}

/// Runs every check against `host` and returns the report. Writes nothing.
#[must_use]
pub fn gather(host: &dyn Host, config: &RunConfig, ops: &OpsFile) -> PreflightReport {
    gather_at(host, config, ops, crate::s3::unix_ms())
}

/// [`gather`], the time now being `now_ms` (milliseconds since the Unix epoch): the clock a
/// status file's stamp is compared with.
#[must_use]
pub fn gather_at(
    host: &dyn Host,
    config: &RunConfig,
    ops: &OpsFile,
    now_ms: u64,
) -> PreflightReport {
    let mut report = PreflightReport::default();
    let (binary, digest) = binary_checks(host, ops, &mut report.snapshot);
    report.binary_digest = digest;
    report
        .tool_versions
        .insert("tierstore".to_owned(), ops.tierstore.version.clone());
    report.checks.extend(binary);
    report
        .checks
        .extend(live_checks(host, ops, &mut report.snapshot));
    let stores = store_checks(host, config, ops, now_ms, &mut report.tool_versions);
    report.checks.extend(stores);
    let node = node_checks(host, config, ops, &mut report.hardware);
    report.checks.extend(node);
    report.checks.extend(paper_checks(host, ops));
    report
}

/// Runs the checks, writes `preflight.json` and `before.json` to the result directory, prints
/// the checks and ends in an error if one failed.
///
/// # Errors
/// [`BenchError`] if the ops file or a report file cannot be read or written, and
/// [`BenchError::Ops`] if a check failed (the files are written first).
pub async fn run(config: &RunConfig) -> Result<PreflightReport, BenchError> {
    let host = SystemHost;
    let ops = OpsFile::load(&host, &config.result_dir)?;
    run_with(&host, config, &ops)
}

/// [`run`] on a given host and ops file.
///
/// # Errors
/// As [`run`], but for the ops file.
pub fn run_with(
    host: &dyn Host,
    config: &RunConfig,
    ops: &OpsFile,
) -> Result<PreflightReport, BenchError> {
    let report = gather(host, config, ops);
    write_json(&config.result_dir.join(REPORT_FILE), &report)?;
    write_json(&config.result_dir.join(SNAPSHOT_FILE), &report.snapshot)?;
    print_checks(&report.checks);
    all_passed("preflight", &report.checks)?;
    Ok(report)
}
