//! The client: one signed unary gRPC call per method, over one tonic channel to one node.
//!
//! The channel connects lazily, on the first call, through [`Dial`]; building a client
//! touches no network. It holds one HTTP/2 connection and multiplexes calls on it, and makes
//! a new one when that breaks. Clones share the channel, so any number of calls may be in
//! flight at once. The client never retries: what a caller does after an error is
//! [`Error::may_have_applied`]'s and the [`Reason`](crate::Reason)'s to say.

use std::sync::Arc;
use std::time::Duration;

use http::uri::PathAndQuery;
use prost::Message;
use tierstore_trees_api::v1::{
    CreateTreeRequest, CreateTreeResponse, DeleteTreeRequest, DeleteTreeResponse,
    GetPlacementRequest, GetPlacementResponse, ListDir,
};
use tonic::Status;
use tonic::client::Grpc;
use tonic::transport::{Channel, Endpoint};

use crate::codec::Lenient;
use crate::config::Config;
use crate::dial::Dial;
use crate::error::{Broken, ConfigError, Error, Undecodable, Unsent, from_status};
use crate::signing::{Signer, Signing};
use crate::types::{Created, PlacementId};

// The path of each method of `tierstore.trees.v1.TreesService`.
const CREATE_TREE: &str = "/tierstore.trees.v1.TreesService/CreateTree";
const GET_PLACEMENT: &str = "/tierstore.trees.v1.TreesService/GetPlacement";
const DELETE_TREE: &str = "/tierstore.trees.v1.TreesService/DeleteTree";

/// A client of one node's trees interface. Cheap to clone; the clones share one channel, so
/// any number of calls may be in flight at once.
#[derive(Clone)]
pub struct Client {
    grpc: Grpc<Signing<Channel>>,
    authority: String,
    tls: bool,
    in_clear_off_loopback: bool,
    /// The most a call may take: connecting, then the exchange.
    deadline: Duration,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("node", &self.authority)
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// The most bytes a request or an answer may hold: tierstore's default
    /// `trees.max_request_bytes`. A bigger request is not sent; a bigger answer is an error,
    /// not a value.
    pub const MAX_MESSAGE: usize = 256 << 20;

    /// A client for `config`. Nothing is dialled until the first call.
    ///
    /// # Errors
    /// A node off loopback with no CA when plaintext is not allowed
    /// ([`ConfigError::Plaintext`]), a CA that holds no usable certificate, or a host a
    /// certificate cannot name.
    ///
    /// # Panics
    /// Outside a Tokio runtime: the channel starts its worker task on the runtime.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let Config {
            node,
            key,
            region,
            ca_pem,
            allow_plaintext,
            connect_timeout,
            call_timeout,
            clock,
        } = config;
        let tls = ca_pem.is_some();
        let in_clear_off_loopback = !tls && !node.is_loopback();
        if in_clear_off_loopback && !allow_plaintext {
            return Err(ConfigError::Plaintext(node.authority().to_owned()));
        }
        let dial = Dial::new(&node, ca_pem.as_deref(), connect_timeout)?;
        let endpoint = Endpoint::from(node.uri(false).clone()).origin(node.uri(tls).clone());
        let channel = endpoint.connect_with_connector_lazy(dial);
        let signer = Arc::new(Signer {
            key,
            region,
            authority: node.authority().to_owned(),
            clock,
        });
        let signing = Signing {
            inner: channel,
            signer,
        };
        let grpc = Grpc::new(signing)
            .max_decoding_message_size(Self::MAX_MESSAGE)
            .max_encoding_message_size(Self::MAX_MESSAGE);
        Ok(Self {
            grpc,
            authority: node.authority().to_owned(),
            tls,
            in_clear_off_loopback,
            deadline: connect_timeout.saturating_add(call_timeout),
        })
    }

    /// Whether the calls go in clear to a node off loopback, which only
    /// [`Config::allow_plaintext`] permits; the caller logs it.
    #[must_use]
    pub fn in_clear_off_loopback(&self) -> bool {
        self.in_clear_off_loopback
    }

    /// Creates a tree and places it (`CreateTree`), and answers the node's digest of it and
    /// its placement.
    ///
    /// A create is idempotent by the tree and its locations: the same request again answers
    /// the placement the first made, with [`Created::is_new`] false. After an error for which
    /// [`Error::may_have_applied`] is true, sending the same request again is safe.
    ///
    /// # Errors
    /// [`Error::Refused`] by name: a request the node will not read (`INVALID_ARGUMENT`), an
    /// `unknown child tree`, a `mismatch` or `short` source (`FAILED_PRECONDITION`), a
    /// `source missing` (`NOT_FOUND`), a location taken (`ALREADY_EXISTS`), a tree too large
    /// for the cache (`RESOURCE_EXHAUSTED`), a busy node (`UNAVAILABLE`), a `create timeout`
    /// (`DEADLINE_EXCEEDED`), a refused key; [`Error::Decode`] for an answer without a
    /// 32-byte digest or a placement; and the transport errors.
    pub async fn create_tree(&self, request: CreateTreeRequest) -> Result<Created, Error> {
        let answer: CreateTreeResponse = self.call(CREATE_TREE, request).await?;
        Created::of(answer).map_err(Error::Decode)
    }

    /// Reads a placement (`GetPlacement`): its tree, state, counts and lease, and with
    /// `list`, one page of the children of one directory. Changes nothing.
    ///
    /// # Errors
    /// [`Error::Refused`] `unknown placement` for a placement the node does not have, and
    /// the transport errors.
    pub async fn get_placement(
        &self,
        placement: &PlacementId,
        list: Option<ListDir>,
    ) -> Result<GetPlacementResponse, Error> {
        let request = GetPlacementRequest {
            placement: placement.as_str().to_owned(),
            list,
        };
        self.call(GET_PLACEMENT, request).await
    }

    /// Releases a placement (`DeleteTree`).
    ///
    /// # Errors
    /// [`Error::Refused`] `unknown placement` for a placement the node does not have, which
    /// a repeat after a lost answer of a delete that applied answers too; and the transport
    /// errors.
    pub async fn delete_tree(&self, placement: &PlacementId) -> Result<(), Error> {
        let request = DeleteTreeRequest {
            placement: placement.as_str().to_owned(),
        };
        let DeleteTreeResponse {} = self.call(DELETE_TREE, request).await?;
        Ok(())
    }

    /// One call of the method at `path`: the request checked against the size cap, signed by
    /// the channel's layer, the whole call under the connect and call timeouts together, and
    /// the answer decoded. A connection not made within the connect timeout fails in the
    /// connector, as nothing sent, before this deadline can pass.
    async fn call<Req, Resp>(&self, path: &'static str, message: Req) -> Result<Resp, Error>
    where
        Req: Message + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
    {
        let len = message.encoded_len();
        if len > Self::MAX_MESSAGE {
            let cap = Self::MAX_MESSAGE;
            return Err(Error::Unreachable(Unsent::TooLarge { len, cap }));
        }
        let request = tonic::Request::new(message);
        let path = PathAndQuery::from_static(path);
        let mut grpc = self.grpc.clone();
        let exchange = async move {
            grpc.ready().await.map_err(Status::from_error)?;
            grpc.unary(request, path, Lenient::<Req, Resp>::new()).await
        };
        match tokio::time::timeout(self.deadline, exchange).await {
            Err(_) => Err(Error::Transport(Broken::Deadline(self.deadline))),
            Ok(Err(status)) => Err(from_status(status)),
            Ok(Ok(answer)) => answer
                .into_inner()
                .map_err(|cause| Error::Decode(Undecodable::Bytes(cause))),
        }
    }
}
