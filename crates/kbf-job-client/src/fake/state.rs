//! The fake's state and the service's rules over it: the jobs, the hosts and offers the test
//! gave, and the clock, which only [`State::advance`] moves.

use std::collections::{BTreeMap, BTreeSet};

use js_proto::refusal::refusal_status;
use js_proto::v1::{
    ExecMode, GetLogsResponse, Job, JobFile, JobResult, JobSpec, JobState, JobTimes,
    ListJobsRequest, ListJobsResponse, NetworkLevel, PortForward, RefusalCode, ResultReason, Shape,
    StartFailureCode,
};
use tonic::Status;

/// The most bytes of each stream a job's tail holds (design section 11).
pub(super) const TAIL: usize = 8192;
/// The page size of a listing that asks for none.
pub(super) const DEFAULT_PAGE: usize = 100;
/// The retention window until [`State::set_retention_ms`]: seven days (design section 3.2).
pub(super) const DEFAULT_RETENTION_MS: u64 = 7 * 24 * 3600 * 1000;
/// The wall-limit ceiling until [`State::set_wall_ceiling_s`]: one day.
pub(super) const DEFAULT_WALL_CEILING_S: u64 = 24 * 3600;

/// What a machine offers: a host the fake has ([`FakeJobServer::add_host`]), or one a
/// provider could launch ([`FakeJobServer::add_offer`]).
///
/// [`FakeJobServer::add_host`]: super::FakeJobServer::add_host
/// [`FakeJobServer::add_offer`]: super::FakeJobServer::add_offer
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// Its resources. A disk of `None` serves no job that asks for disk.
    pub shape: Shape,
    /// Its operating system, as a job's constraint names it (`linux`, `macos`).
    pub os: String,
    /// Its CPU architecture, matched exactly against a job's constraint.
    pub arch: String,
    /// The mode it runs jobs in.
    pub exec: ExecMode,
}

impl Machine {
    /// Whether the machine could ever run `spec`: it has at least each resource the job
    /// needs, and matches each constraint the job gives (an empty one matches any machine).
    fn fits(&self, spec: &JobSpec) -> bool {
        let needs = spec.needs.clone().unwrap_or_default();
        let wants = spec.constraints.clone().unwrap_or_default();
        let have = &self.shape;
        let disk = needs
            .disk_bytes
            .is_none_or(|disk| have.disk_bytes.is_some_and(|room| disk <= room));
        let any_exec = wants.exec() == ExecMode::Unspecified;
        // Each term is one rule of fit; all are evaluated, none short-circuits another.
        [
            needs.cpu_millis <= have.cpu_millis,
            needs.memory_bytes <= have.memory_bytes,
            disk,
            needs.gpus <= have.gpus,
            any_or_same(&needs.gpu_class, &have.gpu_class),
            any_or_same(&wants.os, &self.os),
            any_or_same(&wants.arch, &self.arch),
            any_exec | (wants.exec() == self.exec),
        ]
        .iter()
        .all(|rule| *rule)
    }
}

/// Whether a wanted value is "any" (empty) or the value the machine has.
fn any_or_same(want: &str, have: &str) -> bool {
    want.is_empty() | (want == have)
}

/// A host the fake has.
struct Host {
    machine: Machine,
    /// Since when the host has missed its grace; `None` while it is heard.
    silent_since_ms: Option<u64>,
}

/// One job, as the service keeps it.
struct Record {
    /// The order the job was recorded in, which listings follow.
    seq: u64,
    id: String,
    /// The job as submitted, file contents included: what a host would be sent.
    spec: JobSpec,
    state: JobState,
    start_failure: StartFailureCode,
    /// The agent's own code of a start it refused; empty otherwise.
    start_failure_detail: String,
    host: String,
    times: JobTimes,
    cancel_requested: bool,
    exit_code: Option<i32>,
    reason: ResultReason,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

/// Whether a job in `state` holds a host: placed, starting or running.
fn on_host(state: JobState) -> bool {
    matches!(
        state,
        JobState::Placed | JobState::Starting | JobState::Running
    )
}

/// Whether a job in `state` has not ended.
fn live(state: JobState) -> bool {
    state == JobState::Queued || on_host(state)
}

/// The name the service records on a job submitted with its `--token-file` token, the one
/// token the fake takes (`Job.client`).
pub const CLIENT: &str = "default";

/// The job as a read answers it: the file contents empty, the host's silence shown while
/// the job holds the host, and the client that submitted it.
fn view(record: &Record, hosts: &BTreeMap<String, Host>) -> Job {
    let mut spec = record.spec.clone();
    spec.files.iter_mut().for_each(|file| file.contents.clear());
    let silent = hosts
        .get(&record.host)
        .and_then(|host| host.silent_since_ms)
        .filter(|_| on_host(record.state));
    Job {
        id: record.id.clone(),
        spec: Some(spec),
        state: record.state.into(),
        start_failure: record.start_failure.into(),
        start_failure_detail: record.start_failure_detail.clone(),
        host_id: record.host.clone(),
        times: Some(record.times),
        host_silent_since_ms: silent,
        cancel_requested: record.cancel_requested,
        client: Some(CLIENT.to_owned()),
        queue_status: None,
        attempts: 0,
        events: Vec::new(),
        dropped_events: 0,
    }
}

/// Whether `given` is the job `held` was submitted as, in every field but the priority.
fn same_job(held: &JobSpec, given: &JobSpec) -> bool {
    let mut held = held.clone();
    held.priority = given.priority;
    held == *given
}

/// Ends the job in `state` at `now`. A cancel that was asked for of a job on a host stays
/// marked only on a job that ends lost (`Job.cancel_requested`); [`State::cancel`] marks a
/// queued job it cancels after this.
fn end(record: &mut Record, state: JobState, now: u64) {
    record.state = state;
    record.times.ended_at_ms = Some(now);
    record.cancel_requested &= state == JobState::Lost;
}

/// Appends `bytes` to `tail` and keeps its last [`TAIL`] bytes; whether any were cut.
fn append_tail(tail: &mut Vec<u8>, bytes: &[u8]) -> bool {
    tail.extend_from_slice(bytes);
    let over = tail.len().saturating_sub(TAIL);
    tail.drain(..over);
    over > 0
}

fn not_found(id: &str) -> Status {
    Status::not_found(format!("no job {id}"))
}

/// Whether `image` is pinned by digest: `NAME@sha256:` and 64 lower-case hex digits.
fn pinned(image: &str) -> bool {
    let (name, digest) = image.split_once('@').unwrap_or_default();
    let hex = digest.strip_prefix("sha256:").unwrap_or_default();
    let lower_hex = |b: u8| b.is_ascii_digit() | (b'a'..=b'f').contains(&b);
    !name.is_empty() & (hex.len() == 64) & hex.bytes().all(lower_hex)
}

/// Whether every file name is letters, digits, dot, underscore and dash, starts with a
/// letter or a digit, and is given once.
fn files_named(files: &[JobFile]) -> bool {
    let mut seen = BTreeSet::new();
    files.iter().all(|file| {
        let name = file.name.as_str();
        let first = name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric());
        let rest = name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() | (b == b'.') | (b == b'_') | (b == b'-'));
        first & rest & seen.insert(name)
    })
}

/// Whether every port of every forward is 1 to 65535.
fn ports_in_range(ports: &[PortForward]) -> bool {
    let port = |p: u32| (1..=65_535).contains(&p);
    ports
        .iter()
        .all(|forward| port(forward.host_port) & port(forward.container_port))
}

/// A listing without its page: what a page token is bound to.
fn without_page(request: &ListJobsRequest) -> ListJobsRequest {
    ListJobsRequest {
        page_size: 0,
        page_token: String::new(),
        ..request.clone()
    }
}

/// Whether the job `record` matches the filters of `request` (not its page).
fn listed(record: &Record, request: &ListJobsRequest) -> bool {
    let queued = record.times.queued_at_ms;
    let state = request.states.is_empty() | request.states.contains(&i32::from(record.state));
    let labels = request
        .labels
        .iter()
        .all(|(key, value)| record.spec.labels.get(key) == Some(value));
    let after = request.queued_after_ms.is_none_or(|from| queued >= from);
    let before = request.queued_before_ms.is_none_or(|to| queued < to);
    state & labels & after & before
}

/// The jobs, hosts, offers and clock of the fake.
pub(super) struct State {
    now_ms: u64,
    retention_ms: u64,
    wall_ceiling_s: u64,
    hosts: BTreeMap<String, Host>,
    offers: Vec<Machine>,
    jobs: BTreeMap<String, Record>,
    seq: u64,
    minted: u64,
    /// Every page token given, with the listing (without its page) it continues and the
    /// order number it continues after.
    tokens: BTreeMap<String, (ListJobsRequest, u64)>,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            now_ms: 0,
            retention_ms: DEFAULT_RETENTION_MS,
            wall_ceiling_s: DEFAULT_WALL_CEILING_S,
            hosts: BTreeMap::new(),
            offers: Vec::new(),
            jobs: BTreeMap::new(),
            seq: 0,
            minted: 0,
            tokens: BTreeMap::new(),
        }
    }

    // ---- the clock and the settings

    pub(super) fn advance(&mut self, by_ms: u64) {
        self.now_ms = self.now_ms.saturating_add(by_ms);
        self.sweep();
    }

    pub(super) fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub(super) fn set_retention_ms(&mut self, window: u64) {
        self.retention_ms = window;
        self.sweep();
    }

    pub(super) fn set_wall_ceiling_s(&mut self, ceiling: u64) {
        self.wall_ceiling_s = ceiling;
    }

    /// Forgets the jobs whose retention window has passed: their ids are free again.
    fn sweep(&mut self) {
        let (now, window) = (self.now_ms, self.retention_ms);
        self.jobs.retain(|_, record| {
            let ended = record.times.ended_at_ms;
            ended.is_none_or(|end| now < end.saturating_add(window))
        });
    }

    // ---- hosts and offers

    pub(super) fn add_host(&mut self, id: &str, machine: Machine) {
        let host = Host {
            machine,
            silent_since_ms: None,
        };
        self.hosts.insert(id.to_owned(), host);
    }

    pub(super) fn add_offer(&mut self, machine: Machine) {
        self.offers.push(machine);
    }

    /// Marks the host silent from now (kept if it already was); false for no such host.
    pub(super) fn silence_host(&mut self, id: &str, silent: bool) -> bool {
        let now = self.now_ms;
        let Some(host) = self.hosts.get_mut(id) else {
            return false;
        };
        host.silent_since_ms = silent.then(|| host.silent_since_ms.unwrap_or(now));
        true
    }

    /// Whether a host or an offer could ever run `spec`.
    fn capable(&self, spec: &JobSpec) -> bool {
        let hosts = self.hosts.values().map(|host| &host.machine);
        hosts.chain(&self.offers).any(|machine| machine.fits(spec))
    }

    // ---- the calls

    /// `SubmitJob` (design section 5.3): validate, answer a repeat from the job it made or
    /// refuse a different job under its id, admit, record as queued.
    ///
    /// A network level left unspecified is recorded, and compared and answered, as `none`, the
    /// level the proto reads it as: the released service stores it so, and a repeat that names
    /// `none` or leaves it unspecified is the same job.
    pub(super) fn submit(&mut self, mut spec: JobSpec) -> Result<Job, Status> {
        self.sweep();
        if spec.network == i32::from(NetworkLevel::Unspecified) {
            spec.set_network(NetworkLevel::None);
        }
        if !spec.job_id.is_empty() {
            let checked = crate::id::check(&spec.job_id);
            checked.map_err(|bad| Status::invalid_argument(bad.to_string()))?;
        }
        let rules = [
            (
                pinned(&spec.image),
                "the image is not pinned by digest (NAME@sha256:HEX)",
            ),
            (
                files_named(&spec.files),
                "a file name is not valid, or is given twice",
            ),
            (ports_in_range(&spec.ports), "a port is not 1 to 65535"),
        ];
        if let Some((_, broken)) = rules.into_iter().find(|(kept, _)| !kept) {
            return Err(Status::invalid_argument(broken));
        }
        let wall = spec.wall_limit_seconds.unwrap_or(0);
        if wall == 0 {
            let detail = "the job has no wall limit";
            return Err(refusal_status(RefusalCode::WallTimeNotPositive, detail));
        }
        if wall > self.wall_ceiling_s {
            let ceiling = self.wall_ceiling_s;
            let detail = format!("the wall limit {wall} s is above the ceiling of {ceiling} s");
            return Err(refusal_status(RefusalCode::WallTimeAboveCeiling, detail));
        }
        if let Some(held) = self.jobs.get(&spec.job_id) {
            if same_job(&held.spec, &spec) {
                return Ok(view(held, &self.hosts));
            }
            let detail = format!("{} names another job", spec.job_id);
            return Err(refusal_status(RefusalCode::JobIdConflict, detail));
        }
        if !self.capable(&spec) {
            let detail = "no host and no offer could ever serve the job";
            return Err(refusal_status(RefusalCode::NoCapableHost, detail));
        }
        let id = match spec.job_id.as_str() {
            "" => self.mint(),
            given => given.to_owned(),
        };
        self.seq += 1;
        let record = Record {
            seq: self.seq,
            id: id.clone(),
            spec,
            state: JobState::Queued,
            start_failure: StartFailureCode::Unspecified,
            start_failure_detail: String::new(),
            host: String::new(),
            times: JobTimes {
                queued_at_ms: self.now_ms,
                ..JobTimes::default()
            },
            cancel_requested: false,
            exit_code: None,
            reason: ResultReason::Unspecified,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        };
        let job = view(&record, &self.hosts);
        self.jobs.insert(id, record);
        Ok(job)
    }

    /// A job id no job holds: `j-<counter>`.
    fn mint(&mut self) -> String {
        loop {
            self.minted += 1;
            let id = format!("j-{}", self.minted);
            if !self.jobs.contains_key(&id) {
                return id;
            }
        }
    }

    /// `GetJob`.
    pub(super) fn get(&mut self, id: &str) -> Result<Job, Status> {
        self.sweep();
        let record = self.jobs.get(id).ok_or_else(|| not_found(id))?;
        Ok(view(record, &self.hosts))
    }

    /// `ListJobs`: the matching jobs in the order they were recorded, a page at a time. A
    /// page token is one this fake gave (`p<n>`), bound to the filters of the listing it
    /// continues; any other token, or one given with other filters, is `INVALID_ARGUMENT`.
    pub(super) fn list(&mut self, request: &ListJobsRequest) -> Result<ListJobsResponse, Status> {
        self.sweep();
        let filter = without_page(request);
        let after = match request.page_token.as_str() {
            "" => 0,
            token => match self.tokens.get(token) {
                Some((bound, after)) if *bound == filter => *after,
                _ => {
                    let refused = "the page token is not one this service gave for this listing";
                    return Err(Status::invalid_argument(refused));
                }
            },
        };
        let size = match request.page_size {
            0 => DEFAULT_PAGE,
            asked => asked as usize,
        };
        let mut matching: Vec<&Record> = self
            .jobs
            .values()
            .filter(|record| record.seq > after && listed(record, request))
            .collect();
        matching.sort_by_key(|record| record.seq);
        let more = matching.len() > size;
        matching.truncate(size);
        let last = matching.last().map(|record| record.seq).filter(|_| more);
        let jobs = matching
            .into_iter()
            .map(|record| view(record, &self.hosts))
            .collect();
        let next_page_token = match last {
            Some(seq) => {
                let token = format!("p{}", self.tokens.len() + 1);
                self.tokens.insert(token.clone(), (filter, seq));
                token
            }
            None => String::new(),
        };
        Ok(ListJobsResponse {
            jobs,
            next_page_token,
        })
    }

    /// `CancelJob` (design section 5.3): a queued job is cancelled at once, and has ended, so
    /// it answers without the intent (`cancel_requested`); a job on a host gets the intent and
    /// ends when the host reports it stopped; an ended job is unchanged.
    pub(super) fn cancel(&mut self, id: &str) -> Result<Job, Status> {
        self.sweep();
        let now = self.now_ms;
        let record = self.jobs.get_mut(id).ok_or_else(|| not_found(id))?;
        if record.state == JobState::Queued {
            record.reason = ResultReason::Cancelled;
            end(record, JobState::Cancelled, now);
        } else if on_host(record.state) {
            record.cancel_requested = true;
        }
        Ok(view(record, &self.hosts))
    }

    /// `GetResult`: the result of an ended job, refused for a live one.
    pub(super) fn result(&mut self, id: &str) -> Result<JobResult, Status> {
        self.sweep();
        let record = self.jobs.get(id).ok_or_else(|| not_found(id))?;
        if live(record.state) {
            return Err(Status::failed_precondition(format!(
                "job {id} has not ended, so it has no result"
            )));
        }
        Ok(JobResult {
            state: record.state.into(),
            exit_code: record.exit_code,
            reason: record.reason.into(),
            stdout_tail: record.stdout.clone(),
            stderr_tail: record.stderr.clone(),
            truncated: record.truncated,
            times: Some(record.times),
            usage: None,
        })
    }

    /// `GetLogs` without a stream (the one call the client makes): the job's state, and the
    /// tails of its result once it has one; a live job answers empty tails. A stream, an
    /// offset, a page size and a follow are not read.
    pub(super) fn logs(&mut self, id: &str) -> Result<GetLogsResponse, Status> {
        self.sweep();
        let record = self.jobs.get(id).ok_or_else(|| not_found(id))?;
        let ended = !live(record.state);
        Ok(GetLogsResponse {
            state: record.state.into(),
            stdout_tail: if ended {
                record.stdout.clone()
            } else {
                Vec::new()
            },
            stderr_tail: if ended {
                record.stderr.clone()
            } else {
                Vec::new()
            },
            truncated: ended && record.truncated,
            ..GetLogsResponse::default()
        })
    }

    // ---- what the hosts do

    /// Applies `then` to the job `id` when `when` holds for it; false for no such job or a
    /// job `when` refuses.
    fn step(
        &mut self,
        id: &str,
        when: impl FnOnce(&Record) -> bool,
        then: impl FnOnce(&mut Record, u64),
    ) -> bool {
        self.sweep();
        let now = self.now_ms;
        let Some(record) = self.jobs.get_mut(id) else {
            return false;
        };
        if !when(record) {
            return false;
        }
        then(record, now);
        true
    }

    pub(super) fn place(&mut self, id: &str, host: &str) -> bool {
        if !self.hosts.contains_key(host) {
            return false;
        }
        self.step(
            id,
            |record| record.state == JobState::Queued,
            |record, now| {
                record.state = JobState::Placed;
                record.host = host.to_owned();
                record.times.placed_at_ms = Some(now);
            },
        )
    }

    pub(super) fn start(&mut self, id: &str) -> bool {
        self.step(
            id,
            |record| record.state == JobState::Placed,
            |record, _| record.state = JobState::Starting,
        )
    }

    pub(super) fn run(&mut self, id: &str) -> bool {
        self.step(
            id,
            |record| record.state == JobState::Starting,
            |record, now| {
                record.state = JobState::Running;
                record.times.started_at_ms = Some(now);
            },
        )
    }

    pub(super) fn write_logs(&mut self, id: &str, stdout: &[u8], stderr: &[u8]) -> bool {
        self.step(
            id,
            |record| record.state == JobState::Running,
            |record, _| {
                let cut_out = append_tail(&mut record.stdout, stdout);
                let cut_err = append_tail(&mut record.stderr, stderr);
                record.truncated |= cut_out | cut_err;
            },
        )
    }

    pub(super) fn finish(&mut self, id: &str, exit_code: i32) -> bool {
        self.step(
            id,
            |record| record.state == JobState::Running,
            |record, now| {
                let state = match exit_code {
                    0 => JobState::Succeeded,
                    _ => JobState::Failed,
                };
                record.exit_code = Some(exit_code);
                record.reason = ResultReason::Exit;
                end(record, state, now);
            },
        )
    }

    pub(super) fn fail_to_start(&mut self, id: &str, code: StartFailureCode) -> bool {
        self.fail_to_start_with(id, code, "")
    }

    pub(super) fn fail_to_start_with(
        &mut self,
        id: &str,
        code: StartFailureCode,
        detail: &str,
    ) -> bool {
        self.step(
            id,
            |record| {
                matches!(
                    record.state,
                    JobState::Queued | JobState::Placed | JobState::Starting
                )
            },
            |record, now| {
                record.start_failure = code;
                detail.clone_into(&mut record.start_failure_detail);
                end(record, JobState::FailedToStart, now);
            },
        )
    }

    pub(super) fn lose(&mut self, id: &str) -> bool {
        self.step(
            id,
            |record| on_host(record.state),
            |record, now| end(record, JobState::Lost, now),
        )
    }

    pub(super) fn host_stopped(&mut self, id: &str) -> bool {
        self.step(
            id,
            |record| on_host(record.state) & record.cancel_requested,
            |record, now| {
                record.reason = ResultReason::Cancelled;
                end(record, JobState::Cancelled, now);
            },
        )
    }

    // ---- what the test reads

    /// Every job kept, as a read answers it, in the order they were recorded.
    pub(super) fn jobs(&mut self) -> Vec<Job> {
        self.sweep();
        let mut records: Vec<&Record> = self.jobs.values().collect();
        records.sort_by_key(|record| record.seq);
        records
            .into_iter()
            .map(|record| view(record, &self.hosts))
            .collect()
    }

    /// The job `id` as it was submitted, file contents included.
    pub(super) fn submitted(&mut self, id: &str) -> Option<JobSpec> {
        self.sweep();
        self.jobs.get(id).map(|record| record.spec.clone())
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
