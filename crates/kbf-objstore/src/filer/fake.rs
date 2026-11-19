//! A SeaweedFS filer on loopback that serves the two calls kbf makes of one, for tests:
//! `LookupDirectoryEntries`, with the real filer's refusals (an empty call, more than
//! [`MAX_NAMES_PER_CALL`] names, an empty or relative directory, a name that is empty,
//! `.`, `..` or holds a `/`), and `ListEntries` of one directory. It counts the lookup
//! calls and keeps every name asked; it never sleeps and asserts nothing about time.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::{FutureExt as _, stream};
use kbf_proto::filer_pb::seaweed_filer_server::{SeaweedFiler, SeaweedFilerServer};
use kbf_proto::filer_pb::{
    Entry, FuseAttributes, ListEntriesRequest, ListEntriesResponse, LookupDirectoryEntriesRequest,
    LookupDirectoryEntriesResponse, LookupDirectoryEntryResult,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::codegen::BoxStream;
use tonic::transport::server::TcpIncoming;
use tonic::{Code, Request, Response, Status};

use super::MAX_NAMES_PER_CALL;

/// What the fake holds and what it was asked.
#[derive(Debug, Default)]
struct State {
    /// Entries by full path.
    entries: BTreeMap<String, Entry>,
    /// Results answered as they are for a path, in place of the entry's.
    raw: BTreeMap<String, LookupDirectoryEntryResult>,
    /// The number of names of each lookup call, in the order the calls arrived.
    lookups: Vec<usize>,
    /// Every directory and name asked, in the order they were read.
    asked: Vec<(String, String)>,
    /// The status lookup calls are refused with, when set, and the number of names of the
    /// calls refused (`None`: every call).
    fail: Option<(Option<usize>, Code, String)>,
    /// Whether an answer leaves out its last result.
    drop_last: bool,
}

/// The fake filer (module docs). It serves until it is dropped or the runtime it was
/// started on stops.
#[derive(Debug)]
pub struct FakeFiler {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
    _stop: oneshot::Sender<()>,
}

impl FakeFiler {
    /// Starts the filer on a free loopback port, on the current runtime.
    ///
    /// # Panics
    /// No loopback port could be bound.
    #[expect(
        clippy::expect_used,
        reason = "a test double: a port that cannot be bound is the failure the test reports, as documented under Panics"
    )]
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("address");
        let state = Arc::new(Mutex::new(State::default()));
        let service = Service(Arc::clone(&state));
        let (stop, stopped) = oneshot::channel::<()>();
        let serve = tonic::transport::Server::builder()
            .add_service(SeaweedFilerServer::new(service))
            .serve_with_incoming_shutdown(TcpIncoming::from(listener), stopped.map(drop));
        tokio::spawn(serve);
        Self {
            address,
            state,
            _stop: stop,
        }
    }

    /// `127.0.0.1:<port>`, as [`super::FilerPresence::connect_lazy`] takes it.
    #[must_use]
    pub fn address(&self) -> String {
        self.address.to_string()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Keeps a file of `size` bytes at `path` (absolute, such as `/buckets/b/cas/l/x`).
    pub fn put_file(&self, path: &str, size: u64) {
        let attributes = FuseAttributes {
            file_size: size,
            ..FuseAttributes::default()
        };
        let entry = Entry {
            attributes: Some(attributes),
            ..Entry::default()
        };
        self.put_entry(path, entry);
    }

    /// Keeps `entry` at `path`; its name is set to the path's last segment.
    pub fn put_entry(&self, path: &str, mut entry: Entry) {
        let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
        entry.name = name.to_owned();
        self.state().entries.insert(path.to_owned(), entry);
    }

    /// Answers a lookup of `path` with `result` as it is, whatever entry is kept there.
    pub fn answer_raw(&self, path: &str, result: LookupDirectoryEntryResult) {
        self.state().raw.insert(path.to_owned(), result);
    }

    /// Refuses every lookup call from now on with `code` and `message`.
    pub fn fail_lookups(&self, code: Code, message: &str) {
        self.state().fail = Some((None, code, message.to_owned()));
    }

    /// Refuses, from now on, the lookup calls of exactly `names` names, and serves the others.
    pub fn fail_lookups_of(&self, names: usize, code: Code, message: &str) {
        self.state().fail = Some((Some(names), code, message.to_owned()));
    }

    /// Leaves the last result out of every answer from now on.
    pub fn drop_last_result(&self) {
        self.state().drop_last = true;
    }

    /// The number of names of each lookup call served or refused, in arrival order.
    #[must_use]
    pub fn lookups(&self) -> Vec<usize> {
        self.state().lookups.clone()
    }

    /// Every directory and name the lookups asked, in the order they were read.
    #[must_use]
    pub fn asked(&self) -> Vec<(String, String)> {
        self.state().asked.clone()
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The real filer's check of one call (`validateFilerBatchLookupRequest`).
fn refusal(request: &LookupDirectoryEntriesRequest) -> Option<Status> {
    let count = request.requests.len();
    if count == 0 {
        return Some(Status::invalid_argument(
            "batch lookup requires at least one request",
        ));
    }
    if count > MAX_NAMES_PER_CALL {
        return Some(Status::resource_exhausted(format!(
            "batch lookup has {count} requests; maximum is {MAX_NAMES_PER_CALL}"
        )));
    }
    for (index, asked) in request.requests.iter().enumerate() {
        if !asked.directory.starts_with('/') || asked.directory.contains('\0') {
            let why = format!("batch lookup request {index} has invalid directory");
            return Some(Status::invalid_argument(why));
        }
        let bad_name =
            matches!(asked.name.as_str(), "" | "." | "..") || asked.name.contains(['/', '\0']);
        if bad_name {
            let why = format!("batch lookup request {index} has invalid name");
            return Some(Status::invalid_argument(why));
        }
    }
    None
}

/// `util.JoinPath`: the directory, without a trailing `/`, then `/` and the name.
fn join(directory: &str, name: &str) -> String {
    format!("{}/{name}", directory.trim_end_matches('/'))
}

struct Service(Arc<Mutex<State>>);

#[tonic::async_trait]
impl SeaweedFiler for Service {
    async fn lookup_directory_entries(
        &self,
        request: Request<LookupDirectoryEntriesRequest>,
    ) -> Result<Response<LookupDirectoryEntriesResponse>, Status> {
        let request = request.into_inner();
        let mut state = lock(&self.0);
        state.lookups.push(request.requests.len());
        if let Some((names, code, message)) = &state.fail
            && names.is_none_or(|names| names == request.requests.len())
        {
            return Err(Status::new(*code, message.clone()));
        }
        if let Some(refused) = refusal(&request) {
            return Err(refused);
        }
        let mut results = Vec::with_capacity(request.requests.len());
        for asked in request.requests {
            let path = join(&asked.directory, &asked.name);
            state.asked.push((asked.directory, asked.name));
            let result = match (state.raw.get(&path), state.entries.get(&path)) {
                (Some(raw), _) => raw.clone(),
                (None, Some(entry)) => LookupDirectoryEntryResult {
                    found: true,
                    entry: Some(entry.clone()),
                    ..LookupDirectoryEntryResult::default()
                },
                (None, None) => LookupDirectoryEntryResult::default(),
            };
            results.push(result);
        }
        if state.drop_last {
            results.pop();
        }
        Ok(Response::new(LookupDirectoryEntriesResponse {
            results,
            ..LookupDirectoryEntriesResponse::default()
        }))
    }

    /// The entries directly under the directory, by name, from `startFromFileName`
    /// (included only when asked), at most `limit` of them, and only those whose name begins
    /// with `prefix`. A limit of 0 lists every entry here; the real filer then lists up to its
    /// own `DirListingLimit`, so a caller that wants a bound gives one.
    async fn list_entries(
        &self,
        request: Request<ListEntriesRequest>,
    ) -> Result<Response<BoxStream<ListEntriesResponse>>, Status> {
        let request = request.into_inner();
        let state = lock(&self.0);
        let start = request.start_from_file_name.as_str();
        let limit = match request.limit {
            0 => usize::MAX,
            limit => usize::try_from(limit).unwrap_or(usize::MAX),
        };
        let listed: Vec<Result<ListEntriesResponse, Status>> = state
            .entries
            .iter()
            .filter(|(path, _)| {
                path.rsplit_once('/')
                    .is_some_and(|(parent, _)| parent == request.directory.trim_end_matches('/'))
            })
            .map(|(_, entry)| entry)
            .filter(|entry| entry.name.starts_with(&request.prefix))
            .filter(|entry| match entry.name.as_str().cmp(start) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => request.inclusive_start_from,
                std::cmp::Ordering::Less => false,
            })
            .take(limit)
            .map(|entry| {
                Ok(ListEntriesResponse {
                    entry: Some(entry.clone()),
                    snapshot_ts_ns: 0,
                })
            })
            .collect();
        Ok(Response::new(Box::pin(stream::iter(listed))))
    }
}
