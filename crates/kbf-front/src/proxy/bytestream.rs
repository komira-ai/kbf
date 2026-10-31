//! ByteStream through the frontend as a raw HTTP/2 pass-through.
//!
//! The service is registered under `google.bytestream.ByteStream` and handed the whole
//! HTTP request. It reads the routing key from the first bytes of the request body
//! (`resource_name` is field 1 of `ReadRequest`, `WriteRequest` and
//! `QueryWriteStatusRequest`), authorizes the call against the instance name in it, and
//! gives the request to a storage node with its headers, `grpc-timeout` and body
//! unchanged, except for the map generation header (`kbf-map-generation`): the client's
//! is dropped and the frontend's own is set when it routed by a map, so a node can tell
//! the frontend's map is old. The response (data and trailers) comes back the same way.
//! No protobuf message past that first field is decoded, and no payload byte is
//! collected, so a blob of any size passes in constant memory and nothing here caps one.
//!
//! **Read** tries the owners of the blob in rank order: a node that fails before its
//! first byte (unreachable, an error status in the headers or as the first frame) is
//! passed over, transparently. When no owner answers, the client is told NOT_FOUND only
//! if every owner said so; otherwise it is told the first failure of an owner that could
//! not say (it may hold the blob). Once a byte has been sent the node is the answer: a
//! failure after it ends the call with that error, and the client resumes with
//! `read_offset`.
//!
//! **The hold.** A storage node that restarts is away for a while, so a call that no owner
//! could answer is held and tried again, as the router holds the whole-blob calls
//! ([`kbf_route::Hold`]: the same pauses, 250 ms doubling to 5 s, and the same bound, which
//! is [`Owners::with_hold`]'s). Every try reads the placement again, so an owner that the
//! map marked down and then took back is found. A Read is held while it has not sent a
//! byte: when every owner is down in the map, or when none answered and one of them failed
//! UNAVAILABLE (the others may lack the blob only because the one that is away holds it; a
//! blob every owner said it lacks is NOT_FOUND at once). A Read that has sent its first
//! frame is the node's, as above: the client holds part of the stream and resumes it with
//! `read_offset`, which this does not wait for. A Write is held while it can still be sent
//! whole (below): when the map has no writer for the blob, or every write owner could not
//! be reached before the call went on. Once the client's bytes past the request's first
//! frames have been passed to a node they cannot be sent again (the proxy holds no blob),
//! so a Write in that state is not held. Nothing here waits beyond what the proxy already
//! keeps for failover: those first frames. A QueryWriteStatus is not held: it only says
//! where an upload stands, and a client whose query fails starts the upload again, which is
//! held.
//!
//! **STALE_MAP.** A node that holds a newer map answers FAILED_PRECONDITION with its
//! generation. A Write that can still be sent whole (below) fetches the map and goes to
//! the writer of the newer one, by the router's rule ([`kbf_route::Router::newer_map`]): it
//! stops, UNAVAILABLE, when a fetch brings no newer map or after
//! [`kbf_route::MAX_MAP_REFETCHES`]. A Write past its first frames cannot be sent again, so
//! it ends UNAVAILABLE and the client starts it again, routed by the newer map. A Read and a
//! QueryWriteStatus pass the answer to the client as it is, with no refetch and no second
//! try (the whole-blob calls, which go through the router, do both); the frontend's map is
//! refreshed by its own timer, so the client's next call is routed by the newer map. Only
//! STALE_MAP is treated so: no other answer of a node is sent again here.
//!
//! **Write** goes to the writer: the first owner of the blob that the map does not say is
//! down (not always the owner a read asks first). If the blob's writer changes during the
//! upload it ends UNAVAILABLE and the client starts the upload again: the frontend holds no
//! bytes it could send to the new one. Nothing here hashes what passes: a node checks the
//! blob it is sent and the blob it sends.
//!
//! **A writer that cannot be reached** (dead, but the map has not marked it down yet, which
//! takes the member lease) does not fail the upload if the upload can still be sent whole:
//! when the node refuses the connection or fails with UNAVAILABLE before the call went on
//! (nothing was passed to it but the first frames of the request, which the proxy keeps
//! until the first frame past them is read), the same upload is sent to the next write
//! owner that is not down (`kbf-route`'s `Router::put` does the same for a blob it
//! holds), which takes it as the primary and owes the first owner its copy. Once the
//! client's bytes past the first frames have been read by an attempt, they are gone (the
//! proxy holds no blob), so an attempt that fails after that ends the call with the
//! node's own answer and the client starts again. Only UNAVAILABLE moves on, never an
//! answer from a node that was reached (INVALID_ARGUMENT, a digest that does not match;
//! STALE_MAP has its own rule, above), and nothing is remembered of the node that failed: the
//! next upload of a blob it owns tries it first again. If every owner fails, the first
//! one's failure is the client's, once the hold has ended (also if the map has no writer
//! left by then). Without a map there is one writer, and no next one.
//!
//! **QueryWriteStatus** asks the writer.
//!
//! **Compression.** A gRPC-compressed message hides the name, so a call whose first
//! message is compressed, or that names a `grpc-encoding` other than `identity`, is
//! INVALID_ARGUMENT.
//!
//! **Packs.** Given packs, a Read of a blob the pack index holds is answered from its pack
//! (a blob it lacks is passed on as above), and a Write of a plain blob within the packs'
//! bound is received whole, hashed and packed: the exceptions to "no payload byte is
//! collected" above, which hold one blob. Such a Write first takes its blob's size from the
//! room that the uploads into packs share once the name has been read, and waits for it while
//! they hold it all; it is ended DEADLINE_EXCEEDED if, once it has the room, no byte of its
//! body arrives for the idle timeout (the `packed` module has the rules). Every other call is
//! passed through as before.

use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use kbf_auth::{Authorizers, authorize};
use kbf_route::Waiting;
use kbf_storage::service::{MAP_GENERATION_HEADER, stale_generation};
use kbf_types::Digest;
use tonic::body::Body;
use tonic::codegen::{Service, http};
use tonic::server::NamedService;
use tonic::{Code, Status};

use super::body::{Guard, Pass, Probe, Upload, collect, compressed, name_in};
use super::owners::{Owner, Owners, no_storage};
use super::packed::{self, Packs};
use super::{Direction, Meter, Method, name};

/// The raw ByteStream service.
#[derive(Clone)]
pub(super) struct RawByteStream {
    pub owners: Owners,
    pub authorizers: Arc<Authorizers>,
    pub meter: Arc<Meter>,
    /// The blobs read from and written to packs when there are some (the `packed` module).
    pub packs: Option<Packs>,
}

impl NamedService for RawByteStream {
    const NAME: &'static str = "google.bytestream.ByteStream";
}

impl Service<http::Request<Body>> for RawByteStream {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { Ok(this.serve(request).await.unwrap_or_else(|s| s.into_http())) })
    }
}

/// Whether a node's failure before its first byte says nothing of the request, so the
/// next owner may answer it. A refusal of the request itself (a bad name, a range past
/// the end, no permission) would be the same everywhere.
const fn next_owner_may_answer(code: Code) -> bool {
    matches!(
        code,
        Code::Unavailable | Code::NotFound | Code::Internal | Code::DataLoss | Code::Unknown
    )
}

fn unreachable_node(owner: &Owner, error: &dyn std::fmt::Display) -> Status {
    Status::unavailable(format!("storage node {}: {error}", owner.id()))
}

/// A round of asking the owners of a blob that none of them answered.
struct Unanswered {
    /// What the client is told if the round is the last.
    status: Status,
    /// Whether an owner was away (down in the map, or it failed with anything but
    /// NOT_FOUND), so that another round may find the blob where this one could not.
    away: bool,
}

impl Unanswered {
    /// A node's refusal of the request, which no other round would answer differently.
    fn refused(status: Status) -> Self {
        Self {
            status,
            away: false,
        }
    }
}

impl RawByteStream {
    async fn serve(&self, request: http::Request<Body>) -> Result<http::Response<Body>, Status> {
        let (parts, body) = request.into_parts();
        let method = Method::of_path(parts.uri.path())
            .ok_or_else(|| Status::unimplemented(format!("{} is not served", parts.uri.path())))?;
        let encoding = parts
            .headers
            .get("grpc-encoding")
            .map(http::HeaderValue::as_bytes);
        if encoding.is_some_and(|e| e != b"identity") {
            return Err(compressed("a grpc-encoding other than identity"));
        }
        if method == Method::Write {
            self.write(&parts, body).await
        } else {
            self.unary(&parts, body, method).await
        }
    }

    /// The packs, if there are any and an upload named `name` of the blob `digest` is theirs:
    /// the blob is within their bound and the name is not that of compressed bytes.
    fn packs_for(&self, name: &str, digest: &Digest) -> Option<&Packs> {
        match &self.packs {
            Some(packs) if packed::applies(packs, name, digest) => Some(packs),
            _ => None,
        }
    }

    /// Authorizes the call `method` against the instance in `name`, and says which blob
    /// it is about.
    async fn authorized(
        &self,
        parts: &http::request::Parts,
        method: Method,
        named: &str,
    ) -> Result<name::Resource, Status> {
        let resource = name::parse(method, named)?;
        let authorizer = match method {
            Method::Read => &self.authorizers.cas_get,
            Method::Write | Method::QueryWriteStatus => &self.authorizers.cas_put,
        };
        let caller = kbf_auth::metadata_of_http(&parts.extensions);
        authorize(&**authorizer, &caller, method.path(), &resource.instance).await?;
        Ok(resource)
    }

    async fn write(
        &self,
        parts: &http::request::Parts,
        body: Body,
    ) -> Result<http::Response<Body>, Status> {
        let upload = Upload::new(body);
        let mut up = Pass::new(
            upload.first(),
            &self.meter,
            Method::Write,
            Direction::ToStorage,
        );
        let name = up.peek_name().await?;
        // What was read to find the name is the part of the upload that can be sent again.
        upload.seal();
        let resource = self.authorized(parts, Method::Write, &name).await?;
        if let Some(packs) = self.packs_for(&name, &resource.digest) {
            let body = Body::new(up);
            return packed::write_stream(packs, body, &name, resource.digest).await;
        }
        // The upload waits for an owner while it can be sent whole: nothing of it but the
        // frames above has been read from the client.
        let mut waiting = self.owners.waiting();
        let mut owner = self.writer_held(&resource.digest, &mut waiting).await?;
        // The nodes this upload could not reach, and the first failure.
        let mut skipped: Vec<Arc<str>> = Vec::new();
        let mut first_failure: Option<Status> = None;
        // The map fetches this upload has made after STALE_MAP.
        let mut fetches = 0;
        loop {
            let failed = Arc::new(Mutex::new(None));
            let guarded = up.guarded(Guard {
                owners: self.owners.clone(),
                digest: resource.digest,
                owner: Arc::from(owner.id()),
                skipped: skipped.clone(),
                failed: Arc::clone(&failed),
            });
            let answer = send(&owner, parts, guarded).await;
            // The upload ended because its writer changed: that is the answer, whatever
            // the node said of the body that broke.
            let refusal = failed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(refusal) = refusal {
                return Err(refusal);
            }
            // A node that holds a newer map refused the upload before it read any of it.
            let stale = match (&answer, self.owners.map_router()) {
                (Ok(response), Some(router)) => Status::from_header_map(response.headers())
                    .as_ref()
                    .and_then(stale_generation)
                    .map(|current| (current, router)),
                _ => None,
            };
            if let Some((current, router)) = stale {
                // Fetch the newer map and send the upload to its writer, if the upload
                // can be sent whole; the answer of an upload that cannot is UNAVAILABLE
                // (it starts again, routed by the newer map).
                let Some(body) = upload.again() else {
                    return Err(Status::unavailable(format!(
                        "storage node {} holds map generation {current}, and the upload \
                         is past the part the frontend keeps; start it again",
                        owner.id()
                    )));
                };
                router
                    .newer_map(owner.generation, current, &mut fetches)
                    .await?;
                tracing::warn!(
                    node = owner.id(),
                    current,
                    "storage node holds a newer map; sending the upload by it"
                );
                skipped.clear();
                owner = self.writer_held(&resource.digest, &mut waiting).await?;
                up = Pass::new(body, &self.meter, Method::Write, Direction::ToStorage);
                continue;
            }
            let unreached = match &answer {
                Ok(response) => Status::from_header_map(response.headers())
                    .filter(|status| status.code() == Code::Unavailable),
                Err(status) => (status.code() == Code::Unavailable).then(|| status.clone()),
            };
            if let Some(unreached) = unreached {
                // The node could not take the upload (a refused connection, a reset, an
                // UNAVAILABLE before it read anything). If none of the client's bytes
                // went past the first frames, the next owner is sent the same upload.
                skipped.push(Arc::from(owner.id()));
                let next = self.owners.writer_after(&resource.digest, &skipped);
                if let Some(body) = upload.again() {
                    if let Ok(next) = next {
                        tracing::warn!(node = owner.id(), status = %unreached, "storage node could not take the upload; sending it to the next owner");
                        first_failure.get_or_insert(unreached);
                        owner = next;
                        up = Pass::new(body, &self.meter, Method::Write, Direction::ToStorage);
                        continue;
                    }
                    // Every owner has been tried and the upload can still be sent whole:
                    // wait, then try them all again, first the writer of the map as it is
                    // by then.
                    if waiting.again().await {
                        tracing::warn!(node = owner.id(), status = %unreached, "no storage node could take the upload; holding it");
                        first_failure.get_or_insert(unreached);
                        skipped.clear();
                        // A map that loses its writers meanwhile leaves the node's failure
                        // the client's, not the lookup's.
                        owner = match self.writer_held(&resource.digest, &mut waiting).await {
                            Ok(writer) => writer,
                            Err(status) => return Err(first_failure.unwrap_or(status)),
                        };
                        up = Pass::new(body, &self.meter, Method::Write, Direction::ToStorage);
                        continue;
                    }
                }
                if let Some(first) = first_failure {
                    return Err(first);
                }
            }
            let (head, down) = answer?.into_parts();
            let down = Pass::new(down, &self.meter, Method::Write, Direction::FromStorage);
            return Ok(http::Response::from_parts(head, Body::new(down)));
        }
    }

    /// The node an upload of `digest` goes to, waiting for one while the map has none
    /// (every write owner is down, or the map is gone for the moment). The last answer is
    /// the lookup's own once the hold has ended; a write that has already been refused by
    /// a node is told that node's failure instead (`write`).
    async fn writer_held(
        &self,
        digest: &Digest,
        waiting: &mut Waiting<'_>,
    ) -> Result<Owner, Status> {
        loop {
            let status = match self.owners.writer(digest) {
                Ok(owner) => return Ok(owner),
                Err(status) => status,
            };
            if !waiting.again().await {
                return Err(status);
            }
        }
    }

    /// `Read` and `QueryWriteStatus`: one small request message, replayed to each owner
    /// tried. A read is held before its first byte, as above.
    async fn unary(
        &self,
        parts: &http::request::Parts,
        body: Body,
        method: Method,
    ) -> Result<http::Response<Body>, Status> {
        let request = collect(body).await?;
        let name = name_in(&request)?.ok_or_else(|| {
            Status::invalid_argument("the request is shorter than its resource_name")
        })?;
        let resource = self.authorized(parts, method, &name).await?;
        // The status of an upload is the writer's alone, and not held.
        if method != Method::Read {
            let owners = [self.owners.writer(&resource.digest)?];
            let answered = self.ask_all(&owners, None, parts, &request, method).await;
            return answered.map_err(|unanswered| unanswered.status);
        }
        if let Some(packs) = &self.packs {
            let answered = packed::read_stream(packs, &name, &resource.digest, &request).await?;
            if let Some(response) = answered {
                return Ok(response);
            }
        }
        // A read asks every owner in turn, and while none answers and one is away, again
        // after a pause, against the placement as it is by then.
        let mut waiting = self.owners.waiting();
        loop {
            let (owners, down) = self.owners.placed(&resource.digest);
            let unanswered = match self.ask_all(&owners, down, parts, &request, method).await {
                Ok(response) => return Ok(response),
                Err(unanswered) => unanswered,
            };
            // A blob every owner lacks is not there; it is not worth the wait.
            if !unanswered.away {
                return Err(unanswered.status);
            }
            if !waiting.again().await {
                return Err(unanswered.status);
            }
        }
    }

    /// The first answer of `owners`, asked in turn. NOT_FOUND is the answer only if every
    /// owner the map names said it: an owner that could not say (`down` in the map and so
    /// not asked, or failed any way but NOT_FOUND) may hold the blob, so the first of those
    /// is what the client is told, as the router does for a whole-blob read.
    async fn ask_all(
        &self,
        owners: &[Owner],
        down: Option<Status>,
        parts: &http::request::Parts,
        request: &Bytes,
        method: Method,
    ) -> Result<http::Response<Body>, Unanswered> {
        let (mut failure, mut missed) = (down, None);
        // An owner down in the map, or no owner at all (the map is lost for the moment),
        // is an owner away.
        let mut away = failure.is_some() || owners.is_empty();
        for owner in owners {
            match self.ask(owner, parts, request, method).await {
                Ok(response) => return Ok(response),
                Err(status) if next_owner_may_answer(status.code()) => {
                    tracing::warn!(node = owner.id(), %status, "storage node failed before its first byte");
                    away |= status.code() != Code::NotFound;
                    if status.code() == Code::NotFound {
                        missed = Some(status);
                    } else {
                        // An owner that did not answer is away: the client is told so
                        // (UNAVAILABLE, and it retries), whatever the node failed with.
                        let status = if status.code() == Code::Unavailable {
                            status
                        } else {
                            unreachable_node(owner, &status.message())
                        };
                        failure.get_or_insert(status);
                    }
                }
                // A refusal of the request itself is the same everywhere: no other round
                // would answer it.
                Err(status) => return Err(Unanswered::refused(status)),
            }
        }
        Err(Unanswered {
            status: failure.or(missed).unwrap_or_else(no_storage),
            away,
        })
    }

    /// One owner's answer, once it has failed or sent its first byte.
    async fn ask(
        &self,
        owner: &Owner,
        parts: &http::request::Parts,
        request: &Bytes,
        method: Method,
    ) -> Result<http::Response<Body>, Status> {
        let up = Pass::replay(request.clone(), &self.meter, method, Direction::ToStorage);
        let (head, down) = send(owner, parts, up).await?.into_parts();
        // A status in the headers is a trailers-only answer: the node failed (or, for an
        // empty blob, finished) with nothing to send.
        let failed = Status::from_header_map(&head.headers).filter(|s| s.code() != Code::Ok);
        if let Some(status) = failed {
            return Err(status);
        }
        let mut down = Pass::new(down, &self.meter, method, Direction::FromStorage);
        match down.probe().await {
            Probe::Failed(status) => Err(status),
            Probe::Data | Probe::Done => Ok(http::Response::from_parts(head, Body::new(down))),
        }
    }
}

/// `body` to `owner` as a request like the client's: same method, path, headers.
async fn send(
    owner: &Owner,
    parts: &http::request::Parts,
    body: Pass,
) -> Result<http::Response<tonic::body::Body>, Status> {
    let mut request = http::Request::new(Body::new(body));
    *request.method_mut() = parts.method.clone();
    *request.uri_mut() = parts.uri.clone();
    *request.version_mut() = parts.version;
    *request.headers_mut() = parts.headers.clone();
    // The generation of the map the frontend routed by, never the client's.
    request.headers_mut().remove(MAP_GENERATION_HEADER);
    if owner.generation > 0 {
        request.headers_mut().insert(
            MAP_GENERATION_HEADER,
            http::HeaderValue::from(owner.generation),
        );
    }
    // The channel is a tower Buffer: ready it before calling it.
    let mut channel = owner.channel.clone();
    let response = async {
        poll_fn(|cx| channel.poll_ready(cx)).await?;
        channel.call(request).await
    }
    .await
    .map_err(|e| unreachable_node(owner, &e))?;
    let (head, body) = response.into_parts();
    Ok(http::Response::from_parts(head, Body::new(body)))
}
