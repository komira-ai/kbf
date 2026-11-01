//! The result schema: one JSON per cell, and a CSV of the raw samples.
//!
//! Role names only: no host, address, bucket or key is in any type here, and the types have no
//! free-text field a request error could be copied into except `notes`, which a cell owner fills
//! with prose of its own. The JSON is versioned ([`SCHEMA_VERSION`]); a reader refuses another
//! version rather than guess. The CSV has one row per request, each row carrying the run id,
//! the git SHA and the binary digest, the pass index, the attempt and whether the pass was quiet or busy; the
//! tool versions and the hardware facts by role are in the JSON of the same run (`run_id` joins
//! them).

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cells::CellId;
use crate::config::RateCaps;
use crate::error::BenchError;
use crate::role::Role;

/// The version of the JSON this build writes and reads.
pub const SCHEMA_VERSION: u32 = 1;

/// Whether a pass ran in a drained window or in normal hours. A p99 is claimed from quiet
/// passes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Window {
    /// The farm was drained.
    Quiet,
    /// Normal hours, under the load gate.
    Busy,
}

impl Window {
    /// `quiet` or `busy`, as the CSV writes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Window::Quiet => "quiet",
            Window::Busy => "busy",
        }
    }
}

/// What a request did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Op {
    /// PutObject (create-only unless a cell says otherwise).
    Put,
    /// GetObject, whole.
    Get,
    /// GetObject with a `Range`.
    GetRange,
    /// HeadObject.
    Head,
    /// DeleteObject.
    Delete,
    /// ListObjectsV2.
    List,
    /// CreateMultipartUpload.
    CreateMultipart,
    /// UploadPart.
    UploadPart,
    /// CompleteMultipartUpload.
    CompleteMultipart,
    /// AbortMultipartUpload.
    AbortMultipart,
}

impl Op {
    /// The name the CSV writes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Put => "put",
            Op::Get => "get",
            Op::GetRange => "get-range",
            Op::Head => "head",
            Op::Delete => "delete",
            Op::List => "list",
            Op::CreateMultipart => "create-multipart",
            Op::UploadPart => "upload-part",
            Op::CompleteMultipart => "complete-multipart",
            Op::AbortMultipart => "abort-multipart",
        }
    }
}

/// What a machine of one role is, as the preflight read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    /// Logical cores.
    pub cpu_cores: u32,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// The operating system's name and version.
    pub os: String,
    /// The kernel release.
    pub kernel: String,
    /// The network link speed in Mbit/s, if the interface reports one.
    pub link_mbps: Option<u32>,
}

/// Where a run's numbers come from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Names the run; the CSV rows carry it so they join with this JSON.
    pub run_id: String,
    /// The commit this tool was built from.
    pub git_sha: String,
    /// SHA-256 of the tierstore binary under test.
    pub binary_digest: String,
    /// Versions of the tools used (this tool, the store servers), by name.
    pub tool_versions: BTreeMap<String, String>,
    /// Hardware facts by role.
    pub hardware: BTreeMap<Role, Hardware>,
    /// Which try of the run this is (`run --attempt`, from 1): a retry of a cell and arm keeps
    /// the first try's result file, and the file name and the CSV rows carry the number. A
    /// result written before the field existed was the first try.
    #[serde(default = "first_attempt")]
    pub attempt: u32,
    /// Whether the run added the open-loop arms of the put cells (`run --open-loop`, on unless
    /// set off). A result written before the field existed was a run with them on.
    #[serde(default = "open_loop_on")]
    pub open_loop: bool,
}

/// The open-loop setting of a result that does not name one.
const fn open_loop_on() -> bool {
    true
}

/// The attempt number of a result that does not name one.
const fn first_attempt() -> u32 {
    1
}

impl Provenance {
    /// A provenance with nothing recorded: empty ids and no facts. For the scaffold only: a
    /// result written with it says so by its empty `run_id`, and preflight is to replace it.
    #[must_use]
    pub fn unrecorded() -> Self {
        Self {
            run_id: String::new(),
            git_sha: String::new(),
            binary_digest: String::new(),
            tool_versions: BTreeMap::new(),
            hardware: BTreeMap::new(),
            attempt: first_attempt(),
            open_loop: open_loop_on(),
        }
    }
}

/// One drive of the cold store as the admin info listed it. The drive is named by a short hash
/// of its endpoint, so a result can tell that the set of drives changed between two passes
/// without carrying a host or a path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveState {
    /// `d-` and the first eight hex digits of the SHA-256 of the drive's endpoint.
    pub id: String,
    /// The state the store reports (`ok`, `offline`, ...); empty if it reports none.
    pub state: String,
    /// Whether the drive reports that it is healing.
    pub healing: bool,
}

/// What the cold store was doing when it was read: whether it was healing, its drives, and when
/// the reading was made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColdState {
    /// True if a drive reports healing, or the operator's heal status is under 100%.
    pub healing: bool,
    /// The operator's heal status, percent, if the ops file names a status file.
    pub percent_done: Option<f64>,
    /// The drives the admin info listed, sorted by id.
    pub drives: Vec<DriveState>,
    /// When it was read, milliseconds since the Unix epoch: by this tool for `mc`, by the
    /// writer of the status file (its own stamp) for `file`.
    pub read_unix_ms: u64,
    /// Where the reading came from; absent in results written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ColdSource>,
}

/// Where a [`ColdState`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColdSource {
    /// The operator's heal status file, stamped by its writer.
    File,
    /// `mc admin info` of the cold role.
    Mc,
}

/// The reading taken before one pass of an arm.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassColdState {
    /// The pass it was taken before.
    pub pass: u32,
    /// What was read.
    #[serde(flatten)]
    pub state: ColdState,
}

/// The cold store's health guard tripping, see [`crate::cells::coldguard`]: which measure of the
/// harness's own cold requests went over its limit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error(
    "cold store guard: {metric} is {value}, over the limit {limit} (over {requests} requests of the last {window_secs} s)"
)]
pub struct ColdTrip {
    /// `p99_ms` or `error_share`.
    pub metric: String,
    /// What the window said.
    pub value: f64,
    /// The limit it exceeded.
    pub limit: f64,
    /// Cold requests in the window when it tripped.
    pub requests: u64,
    /// The length of the window, seconds.
    pub window_secs: u64,
}

/// Latency percentiles of one pass, in microseconds (nearest rank).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Percentiles {
    /// Median.
    pub p50_us: u64,
    /// 95th percentile.
    pub p95_us: u64,
    /// 99th percentile; `None` unless the pooled quiet sample count allows the claim
    /// (1,000 requests).
    pub p99_us: Option<u64>,
    /// Slowest request.
    pub max_us: u64,
}

/// One pass of one arm of a cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassResult {
    /// Index from 0; pass 0 is the warm-up and is `discarded`.
    pub pass: u32,
    /// Quiet or busy.
    pub window: Window,
    /// True for the warm-up pass: reported in the file, left out of every figure.
    pub discarded: bool,
    /// Requests made.
    pub requests: u64,
    /// Requests that failed (the status counts are in the CSV).
    pub failures: u64,
    /// Bytes moved in the pass.
    pub bytes: u64,
    /// Wall time of the pass, microseconds.
    pub elapsed_us: u64,
    /// `None` if the pass made no request that completed.
    pub percentiles: Option<Percentiles>,
    /// The load gate's measure that aborted the pass, if one did.
    pub gate_tripped: Option<String>,
    /// The cold store guard's trip that aborted the pass, if one did.
    #[serde(default)]
    pub cold_guard: Option<ColdTrip>,
    /// Requests the fault proxy forwarded, by HTTP method, while the pass was driven and while
    /// the arm's cleanup hook ran; none when the arm has no proxy in its path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_requests: Option<ProxyRequests>,
    /// The busy time of each whole disk of the machine that sends the requests over the pass,
    /// by device name; empty when the machine could not be read for it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub disk_busy: BTreeMap<String, DiskBusy>,
    /// The most requests that were in flight at the start of one request of the pass: what the
    /// pass realised of the width it was asked for.
    #[serde(default)]
    pub peak_in_flight: u32,
    /// What the machine that sends the requests looked like while the pass ran: the readings
    /// taken inside it. Recorded, never a reason to stop the pass; `None` when none was taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_load: Option<PassLoad>,
}

/// The readings of the load inside one pass. Each is taken over the interval since the previous
/// reading (the end of the quiet window before the pass for the first), and the pass's own
/// requests are in all of them: the figures say what the machine did, not whose work it was.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassLoad {
    /// How many readings were taken.
    pub readings: u32,
    /// The highest share of an interval with some task stalled on CPU, percent.
    pub peak_psi_cpu_pct: f64,
    /// The highest share of an interval with some task stalled on IO, percent.
    pub peak_psi_io_pct: f64,
    /// The highest utilisation of the busiest disk over an interval, percent.
    pub peak_disk_util_pct: f64,
}

/// What the fault proxy forwarded to the store around one pass, by HTTP method. A method with no
/// request is absent. Background traffic (a copy the instance makes later) is not separated.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyRequests {
    /// While the pass sent its requests.
    pub during: BTreeMap<String, u64>,
    /// The requests made while the arm's cleanup hook ran: the bench's own cleanup, plus any
    /// background traffic that happened to land then. Background traffic is not separated. An
    /// arm with no cleanup hook has about none. Older result files call this field `after`.
    #[serde(alias = "after")]
    pub cleanup: BTreeMap<String, u64>,
}

/// How busy one disk was over a pass.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskBusy {
    /// Milliseconds the disk spent doing IO during the pass (`io_ticks` of `/proc/diskstats`).
    pub busy_ms: u64,
    /// `busy_ms` over the wall time of the pass, percent, at most 100.
    pub util_pct: f64,
}

/// The result of one arm of one cell: the contents of one result JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellResult {
    /// [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The cell.
    pub cell: CellId,
    /// The arm (`healthy`, `cold-slow-50ms`, ...): a label of the cell owner's, role names and
    /// numbers only.
    pub arm: String,
    /// Start, milliseconds since the Unix epoch.
    pub started_unix_ms: u64,
    /// End, milliseconds since the Unix epoch.
    pub finished_unix_ms: u64,
    /// Where the numbers come from.
    pub provenance: Provenance,
    /// Every pass, the discarded one included.
    pub passes: Vec<PassResult>,
    /// Prose of the cell owner's: caveats a reader needs. No hosts, buckets or keys.
    pub notes: Vec<String>,
    /// What the cold store was doing before each pass, read again every time: empty if the run
    /// had no cold store to read (or the result was written before the field existed).
    #[serde(default)]
    pub cold_state: Vec<PassColdState>,
    /// The request-rate caps the run held every client to. A cell whose requests wait at a cap
    /// measures the cap, so the caps are in every result. Absent in a result written before the
    /// field existed.
    #[serde(default)]
    pub rate_caps: Option<RateCaps>,
}

impl CellResult {
    /// The JSON, indented, ending in a newline.
    ///
    /// # Panics
    /// Never: every field serialises.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a result serialises");
        text.push('\n');
        text
    }

    /// Parses a result file.
    ///
    /// # Errors
    /// [`BenchError::Result`] if the text is not a result or has another schema version.
    pub fn from_json(text: &str) -> Result<Self, BenchError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| BenchError::Result(e.to_string()))?;
        let version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(SCHEMA_VERSION)) {
            return Err(BenchError::Result(format!(
                "schema_version is {version:?}, this build reads {SCHEMA_VERSION}"
            )));
        }
        serde_json::from_value(value).map_err(|e| BenchError::Result(e.to_string()))
    }
}

/// The outcome of one check of a preflight or a teardown verification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// What was checked, in role names.
    pub name: String,
    /// PASS or FAIL.
    pub passed: bool,
    /// What was seen: counts and hashes, never contents.
    pub detail: String,
}

/// One request, as a CSV row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// The cell.
    pub cell: CellId,
    /// The arm.
    pub arm: String,
    /// The pass index.
    pub pass: u32,
    /// Quiet or busy.
    pub window: Window,
    /// The store the request went to.
    pub role: Role,
    /// What it did.
    pub op: Op,
    /// Payload bytes of the request or the answer.
    pub bytes: u64,
    /// Requests in flight when it was sent, itself included.
    pub in_flight: u32,
    /// When it was sent, milliseconds since the Unix epoch.
    pub started_unix_ms: u64,
    /// How long it took, microseconds.
    pub latency_us: u64,
    /// The HTTP status, if the request got an answer.
    pub status: Option<u16>,
}

impl Sample {
    /// The header row, without a newline.
    pub const CSV_HEADER: &'static str = "run_id,git_sha,binary_digest,cell,arm,pass,window,role,op,bytes,in_flight,started_unix_ms,latency_us,status,attempt";

    /// The row for this sample, without a newline, in the order of [`Sample::CSV_HEADER`].
    #[must_use]
    pub fn csv_row(&self, provenance: &Provenance) -> String {
        let status = self.status.map_or_else(String::new, |s| s.to_string());
        let fields: [Cow<'_, str>; 15] = [
            csv_field(&provenance.run_id),
            csv_field(&provenance.git_sha),
            csv_field(&provenance.binary_digest),
            Cow::Borrowed(self.cell.as_str()),
            csv_field(&self.arm),
            Cow::Owned(self.pass.to_string()),
            Cow::Borrowed(self.window.as_str()),
            Cow::Borrowed(self.role.as_str()),
            Cow::Borrowed(self.op.as_str()),
            Cow::Owned(self.bytes.to_string()),
            Cow::Owned(self.in_flight.to_string()),
            Cow::Owned(self.started_unix_ms.to_string()),
            Cow::Owned(self.latency_us.to_string()),
            Cow::Owned(status),
            Cow::Owned(provenance.attempt.to_string()),
        ];
        fields.join(",")
    }
}

/// A field quoted if it holds a comma, a quote or a line break (RFC 4180).
fn csv_field(s: &str) -> Cow<'_, str> {
    if s.contains([',', '"', '\n', '\r']) {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else {
        Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance {
            run_id: "run-1".to_owned(),
            git_sha: "0123abc".to_owned(),
            binary_digest: "ff00".to_owned(),
            tool_versions: BTreeMap::from([("tierstore".to_owned(), "0.0.37".to_owned())]),
            hardware: BTreeMap::from([(
                Role::Instance,
                Hardware {
                    cpu_cores: 16,
                    memory_bytes: 64 << 30,
                    os: "linux".to_owned(),
                    kernel: "6.1".to_owned(),
                    link_mbps: Some(10_000),
                },
            )]),
            attempt: 2,
            open_loop: true,
        }
    }

    fn result() -> CellResult {
        CellResult {
            schema_version: SCHEMA_VERSION,
            cell: CellId::A1,
            arm: "healthy".to_owned(),
            started_unix_ms: 1_000,
            finished_unix_ms: 2_000,
            provenance: provenance(),
            passes: vec![PassResult {
                pass: 0,
                window: Window::Quiet,
                discarded: true,
                requests: 10,
                failures: 1,
                bytes: 4096,
                elapsed_us: 5_000,
                percentiles: Some(Percentiles {
                    p50_us: 100,
                    p95_us: 200,
                    p99_us: None,
                    max_us: 300,
                }),
                gate_tripped: Some("loadavg_1m".to_owned()),
                cold_guard: Some(ColdTrip {
                    metric: "p99_ms".to_owned(),
                    value: 2500.0,
                    limit: 2000.0,
                    requests: 40,
                    window_secs: 60,
                }),
                proxy_requests: Some(ProxyRequests {
                    during: BTreeMap::from([("HEAD".to_owned(), 3)]),
                    cleanup: BTreeMap::from([("DELETE".to_owned(), 10)]),
                }),
                disk_busy: BTreeMap::from([(
                    "sda".to_owned(),
                    DiskBusy {
                        busy_ms: 2_500,
                        util_pct: 50.0,
                    },
                )]),
                peak_in_flight: 7,
                pass_load: Some(PassLoad {
                    readings: 3,
                    peak_psi_cpu_pct: 1.5,
                    peak_psi_io_pct: 41.0,
                    peak_disk_util_pct: 100.0,
                }),
            }],
            notes: vec!["a note".to_owned()],
            cold_state: vec![PassColdState {
                pass: 0,
                state: ColdState {
                    healing: true,
                    percent_done: Some(12.5),
                    drives: vec![DriveState {
                        id: "d-0123abcd".to_owned(),
                        state: "ok".to_owned(),
                        healing: true,
                    }],
                    read_unix_ms: 1_500,
                    source: Some(ColdSource::File),
                },
            }],
            rate_caps: Some(RateCaps {
                cold_rps: 20,
                hot_rps: 50,
                list_poll_rps: 2,
            }),
        }
    }

    /// Catches a field lost or renamed between writing a result and reading it back.
    #[test]
    fn a_result_round_trips_through_its_json() {
        let r = result();
        let text = r.to_json();
        assert!(text.ends_with("}\n"));
        assert_eq!(CellResult::from_json(&text).unwrap(), r);
    }

    /// Catches the open-loop setting of a run not being kept in the provenance of its results,
    /// and a result written before the field existed being read as a run without the arms.
    #[test]
    fn the_open_loop_setting_is_in_the_provenance_and_an_old_result_reads_as_on() {
        let mut r = result();
        r.provenance.open_loop = false;
        let mut v: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(v["provenance"]["open_loop"], false);
        assert_eq!(CellResult::from_json(&v.to_string()).unwrap(), r);
        v["provenance"].as_object_mut().unwrap().remove("open_loop");
        assert!(
            CellResult::from_json(&v.to_string())
                .unwrap()
                .provenance
                .open_loop
        );
    }

    /// Pins the name of the cleanup counts and that a result written under the old name `after`
    /// still parses into it.
    #[test]
    fn the_cleanup_counts_are_named_cleanup_and_the_old_name_still_parses() {
        let v: serde_json::Value = serde_json::from_str(&result().to_json()).unwrap();
        let counts = &v["passes"][0]["proxy_requests"];
        assert_eq!(counts["cleanup"]["DELETE"], 10);
        assert!(counts.get("after").is_none());
        let old = v.to_string().replace("\"cleanup\"", "\"after\"");
        let parsed = CellResult::from_json(&old).unwrap();
        let counts = parsed.passes[0].proxy_requests.as_ref().unwrap();
        assert_eq!(counts.cleanup["DELETE"], 10);
    }

    /// Catches a result written before the cold state, the cold guard, the attempt, the rate caps,
    /// the proxy counts, the disk busy time and the peak in flight were added
    /// being refused, or read as something other than a first attempt with no readings.
    #[test]
    fn a_result_without_the_newer_fields_reads_with_their_defaults() {
        let mut v: serde_json::Value = serde_json::from_str(&result().to_json()).unwrap();
        v.as_object_mut().unwrap().remove("cold_state");
        v.as_object_mut().unwrap().remove("rate_caps");
        v["provenance"].as_object_mut().unwrap().remove("attempt");
        for pass in v["passes"].as_array_mut().unwrap() {
            for field in [
                "cold_guard",
                "proxy_requests",
                "disk_busy",
                "peak_in_flight",
                "pass_load",
            ] {
                pass.as_object_mut().unwrap().remove(field);
            }
        }
        let old = CellResult::from_json(&v.to_string()).unwrap();
        assert!(old.cold_state.is_empty());
        assert_eq!(old.rate_caps, None);
        assert_eq!(
            (
                old.passes[0].proxy_requests.is_none(),
                old.passes[0].disk_busy.is_empty(),
                old.passes[0].peak_in_flight,
                old.passes[0].pass_load.is_none()
            ),
            (true, true, 0, true)
        );
        assert_eq!(old.provenance.attempt, 1);
        assert_eq!(old.passes[0].cold_guard, None);
    }

    /// Catches a cold reading whose fields are not flat beside the pass (the shape the report and
    /// the paper scripts read) or that carries a name of the store.
    #[test]
    fn a_cold_reading_is_flat_and_names_no_host() {
        let v: serde_json::Value = serde_json::from_str(&result().to_json()).unwrap();
        let reading = &v["cold_state"][0];
        assert_eq!(reading["pass"], 0);
        assert_eq!(reading["healing"], true);
        assert_eq!(reading["read_unix_ms"], 1_500);
        assert_eq!(reading["drives"][0]["id"], "d-0123abcd");
    }

    /// Catches a reader that accepts another schema version, or no version, and reads it as
    /// this one.
    #[test]
    fn another_schema_version_or_none_is_refused() {
        let mut v: serde_json::Value = serde_json::from_str(&result().to_json()).unwrap();
        v["schema_version"] = 2.into();
        let err = CellResult::from_json(&v.to_string()).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "{err}");
        v.as_object_mut().unwrap().remove("schema_version");
        assert!(CellResult::from_json(&v.to_string()).is_err());
        assert!(CellResult::from_json("not json").is_err());
        assert!(CellResult::from_json(r#"{"schema_version": 1}"#).is_err());
    }

    /// Catches a host-like string in what the schema carries: the role names, the cell names and
    /// the JSON of a result must hold no address, no port and no `://`.
    #[test]
    fn the_names_and_a_result_hold_nothing_host_like() {
        let mut text = result().to_json();
        for role in Role::ALL {
            text.push_str(role.as_str());
        }
        for cell in CellId::ALL {
            text.push_str(cell.as_str());
        }
        assert!(!text.contains("://"));
        let dotted_quad = text
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter(|w| w.split('.').count() == 4)
            .any(|w| w.split('.').all(|p| p.parse::<u8>().is_ok()));
        assert!(!dotted_quad);
    }

    /// Catches a row out of step with the header, an unquoted comma, and an empty status.
    #[test]
    fn a_csv_row_has_the_header_s_columns_and_quotes_what_needs_it() {
        let mut s = Sample {
            cell: CellId::B,
            arm: "a,\"b\"".to_owned(),
            pass: 2,
            window: Window::Busy,
            role: Role::Cold,
            op: Op::GetRange,
            bytes: 4096,
            in_flight: 16,
            started_unix_ms: 99,
            latency_us: 1234,
            status: None,
        };
        let row = s.csv_row(&provenance());
        assert_eq!(
            row,
            "run-1,0123abc,ff00,b,\"a,\"\"b\"\"\",2,busy,cold,get-range,4096,16,99,1234,,2"
        );
        assert_eq!(Sample::CSV_HEADER.split(',').count(), 15);
        s.arm = "plain".to_owned();
        s.status = Some(206);
        assert!(s.csv_row(&provenance()).ends_with(",1234,206,2"));
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
    }

    /// Catches an operation or window whose CSV name and JSON name differ.
    #[test]
    fn csv_names_match_json_names() {
        for op in [
            Op::Put,
            Op::Get,
            Op::GetRange,
            Op::Head,
            Op::Delete,
            Op::List,
            Op::CreateMultipart,
            Op::UploadPart,
            Op::CompleteMultipart,
            Op::AbortMultipart,
        ] {
            assert_eq!(
                serde_json::to_string(&op).unwrap(),
                format!("\"{}\"", op.as_str())
            );
        }
        for w in [Window::Quiet, Window::Busy] {
            assert_eq!(
                serde_json::to_string(&w).unwrap(),
                format!("\"{}\"", w.as_str())
            );
        }
    }
}
