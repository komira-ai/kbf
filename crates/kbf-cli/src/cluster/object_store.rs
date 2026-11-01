//! `kbf cluster object-store ...`: the verbs of `/v1/object-stores/...` (`docs/api.md`),
//! each one call. They register the farm's object store with the frontend: a connection
//! (an endpoint and a key pair), a binding of the `cold` role to a connection, a bucket
//! and a prefix, a probe, the desired state with its convergence, and the audit log.
//!
//! - `status`: `GET /v1/object-stores`.
//! - `audit [--limit N]`: `GET /v1/object-stores/audit`.
//! - `connection list|show NAME|put NAME ...|delete NAME`: `/connections[/NAME]`.
//! - `bind --connection NAME --bucket B [--prefix P]` and `unbind`: `PUT` and `DELETE`
//!   of `/bindings/cold`.
//! - `verify NAME [--bucket B] [--prefix P]`: `POST /connections/NAME:verify`.
//! - `repoint --connection NAME --endpoint URL [--bucket B] [--prefix P] --expect-generation N`:
//!   `POST /connections/NAME:repoint`.
//!
//! Every route passes the whole write gate, a `GET` included, so every verb needs
//! `--token-file`.
//!
//! **The secret key** reaches `connection put` from exactly one of `--secret-stdin` (the
//! key is piped in) or `--secret-file FILE` (a regular file whose mode gives no access to
//! its group or others; the check is on the opened file, so a swap after it changes
//! nothing). It is never a flag, so it is in neither `ps` nor the shell history, and never
//! an environment variable. Only the final line ending is cut. The key is held in a
//! buffer that is zeroed when dropped and appears in nothing the command prints: an error
//! prints the API's status and answer, and the API's answers hold no secret.
//! The request text passes through the operating system's socket buffers and the
//! request `String` of [`kbf_server::api_client::OperatorClient`], which is not zeroed.

use std::io::Read;
use std::num::{NonZeroU64, NonZeroUsize};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use kbf_server::object_store::api::BASE;
use kbf_types::object_store_api::{
    BindingPut, BindingRole, ConnectionName, RepointRequest, VerifyRequest,
};
use serde::Serialize;
use zeroize::Zeroizing;

use super::{Body, Call, CallError};

/// The most bytes read from a secret source. A key is at most 1024 characters
/// (`docs/api.md`); the rest is room for a line ending and for a wrong file to be named
/// as one instead of being read to its end.
const SECRET_READ_MAX: u64 = 4096;

/// The file holding the server's operator API token.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The file holding the server's operator API token (every object-store route
    /// passes the whole write gate, a read too).
    #[arg(long, value_name = "FILE")]
    pub token_file: PathBuf,
}

/// Where `connection put` reads the secret key: one of the two, never a flag value.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
#[group(required = true, multiple = false)]
pub struct SecretSource {
    /// Read the secret key from this file: a regular file only its owner can access (mode
    /// 0600 or 0400).
    #[arg(long, value_name = "FILE")]
    pub secret_file: Option<PathBuf>,
    /// Read the secret key from standard input (pipe it in).
    #[arg(long)]
    pub secret_stdin: bool,
}

/// What `kbf cluster object-store` does.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ObjectStore {
    /// Show the desired state (connections, bindings) and whether the nodes have applied it.
    Status {
        #[command(flatten)]
        token: Token,
    },
    /// Show the newest audit rows of the registration's writes.
    Audit {
        /// How many rows (1 to 500; the server's default, 50, if not given).
        #[arg(long, value_name = "N")]
        limit: Option<NonZeroUsize>,
        #[command(flatten)]
        token: Token,
    },
    /// List, show, register or delete a connection: an endpoint and a key pair.
    Connection {
        #[command(subcommand)]
        verb: Connection,
    },
    /// Bind a role to a connection, a bucket and a prefix (only `cold` is supported).
    Bind {
        /// The role; `cold` is today's `--cold-store`.
        #[arg(long, default_value = "cold")]
        role: BindingRole,
        /// The connection the role uses.
        #[arg(long)]
        connection: ConnectionName,
        /// The bucket.
        #[arg(long)]
        bucket: String,
        /// The key prefix in the bucket (none if not given).
        #[arg(long)]
        prefix: Option<String>,
        /// Refuse the write if the registry is not at this generation.
        #[arg(long, value_name = "N")]
        expect_generation: Option<u64>,
        /// Write without probing the bucket first.
        #[arg(long)]
        no_verify: bool,
        #[command(flatten)]
        token: Token,
    },
    /// Remove a role's binding (refused once a node has applied it).
    Unbind {
        /// The role.
        #[arg(long, default_value = "cold")]
        role: BindingRole,
        #[command(flatten)]
        token: Token,
    },
    /// Move the store the `cold` binding names to another endpoint (and bucket and prefix),
    /// on purpose: the new store is probed with the connection's key first, and a failed
    /// probe changes nothing. Only nodes started with `--cold-store registered` follow.
    Repoint {
        /// The bound connection.
        #[arg(long)]
        connection: ConnectionName,
        /// The new store's address, `http://host[:port]`.
        #[arg(long)]
        endpoint: String,
        /// The new bucket (the binding's current one if not given).
        #[arg(long)]
        bucket: Option<String>,
        /// The new key prefix (the binding's current one if not given).
        #[arg(long)]
        prefix: Option<String>,
        /// Refuse the change if the registry is not at this generation (read it with
        /// `status`).
        #[arg(long, value_name = "N")]
        expect_generation: u64,
        #[command(flatten)]
        token: Token,
    },
    /// Probe a connection from the frontend: write, read and delete one small object.
    Verify {
        /// The connection.
        name: ConnectionName,
        /// The bucket to probe (the `cold` binding's if not given).
        #[arg(long)]
        bucket: Option<String>,
        /// The prefix to probe under (the `cold` binding's if not given).
        #[arg(long)]
        prefix: Option<String>,
        #[command(flatten)]
        token: Token,
    },
}

/// What `kbf cluster object-store connection` does.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum Connection {
    /// List the connections (never a secret).
    List {
        #[command(flatten)]
        token: Token,
    },
    /// Show one connection (never a secret).
    Show {
        /// The connection.
        name: ConnectionName,
        #[command(flatten)]
        token: Token,
    },
    /// Create a connection, or replace its credentials; the same call again changes nothing.
    Put {
        /// The connection (a-z, 0-9, `_` and `-`, up to 63 characters).
        name: ConnectionName,
        /// The store's address, `http://host[:port]`.
        #[arg(long)]
        endpoint: String,
        /// The access key id (not secret).
        #[arg(long)]
        access_key_id: String,
        #[command(flatten)]
        secret: SecretSource,
        /// The signing region (the store's default if not given).
        #[arg(long)]
        region: Option<String>,
        /// The store refuses to overwrite a key.
        #[arg(long)]
        conditional_put: bool,
        /// The slowest request body rate, in bytes a second (the default if not given).
        #[arg(long, value_name = "BYTES")]
        min_put_rate: Option<NonZeroU64>,
        /// Refuse the write if the registry is not at this generation.
        #[arg(long, value_name = "N")]
        expect_generation: Option<u64>,
        /// Write without probing the bound bucket with the key first.
        #[arg(long)]
        no_verify: bool,
        #[command(flatten)]
        token: Token,
    },
    /// Delete a connection (refused while a binding names it).
    Delete {
        /// The connection.
        name: ConnectionName,
        #[command(flatten)]
        token: Token,
    },
}

impl ObjectStore {
    /// The token file every verb of the group needs.
    #[must_use]
    pub fn token_file(&self) -> &PathBuf {
        match self {
            Self::Status { token }
            | Self::Audit { token, .. }
            | Self::Bind { token, .. }
            | Self::Unbind { token, .. }
            | Self::Repoint { token, .. }
            | Self::Verify { token, .. } => &token.token_file,
            Self::Connection { verb } => verb.token_file(),
        }
    }

    /// The call this verb makes. `stdin` is read only by `connection put --secret-stdin`.
    ///
    /// # Errors
    /// The secret key cannot be read from its source, or is empty.
    pub fn call(&self, stdin: &mut dyn Read) -> Result<Call, CallError> {
        Ok(match self {
            Self::Status { .. } => Call::GetGated(BASE.to_owned()),
            Self::Audit { limit, .. } => Call::GetGated(limit.map_or_else(
                || format!("{BASE}/audit"),
                |limit| format!("{BASE}/audit?limit={limit}"),
            )),
            Self::Connection { verb } => verb.call(stdin)?,
            Self::Bind {
                role,
                connection,
                bucket,
                prefix,
                expect_generation,
                no_verify,
                ..
            } => Call::Put(
                format!("{BASE}/bindings/{role}"),
                Body::json(&BindingPut {
                    connection: connection.clone(),
                    bucket: bucket.clone(),
                    prefix: prefix.clone().unwrap_or_default(),
                    expect_generation: *expect_generation,
                    verify: no_verify.then_some(false),
                }),
            ),
            Self::Unbind { role, .. } => Call::Delete(format!("{BASE}/bindings/{role}")),
            Self::Repoint {
                connection,
                endpoint,
                bucket,
                prefix,
                expect_generation,
                ..
            } => Call::Post(
                format!("{BASE}/connections/{connection}:repoint"),
                Body::json(&RepointRequest {
                    endpoint: endpoint.clone(),
                    bucket: bucket.clone(),
                    prefix: prefix.clone(),
                    expect_generation: *expect_generation,
                }),
            ),
            Self::Verify {
                name,
                bucket,
                prefix,
                ..
            } => {
                let request = VerifyRequest {
                    bucket: bucket.clone(),
                    prefix: prefix.clone(),
                };
                let body = if request == VerifyRequest::default() {
                    // The API takes no body as the request that uses the binding's store.
                    Body::empty()
                } else {
                    Body::json(&request)
                };
                Call::Post(format!("{BASE}/connections/{name}:verify"), body)
            }
        })
    }
}

impl Connection {
    fn token_file(&self) -> &PathBuf {
        match self {
            Self::List { token }
            | Self::Show { token, .. }
            | Self::Put { token, .. }
            | Self::Delete { token, .. } => &token.token_file,
        }
    }

    fn call(&self, stdin: &mut dyn Read) -> Result<Call, CallError> {
        Ok(match self {
            Self::List { .. } => Call::GetGated(format!("{BASE}/connections")),
            Self::Show { name, .. } => Call::GetGated(format!("{BASE}/connections/{name}")),
            Self::Delete { name, .. } => Call::Delete(format!("{BASE}/connections/{name}")),
            Self::Put {
                name,
                endpoint,
                access_key_id,
                secret,
                region,
                conditional_put,
                min_put_rate,
                expect_generation,
                no_verify,
                ..
            } => {
                let secret = read_secret(secret, stdin)?;
                Call::Put(
                    format!("{BASE}/connections/{name}"),
                    Body::json(&ConnectionBody {
                        endpoint,
                        region: region.as_deref(),
                        access_key_id,
                        secret_access_key: &secret,
                        conditional_put: conditional_put.then_some(true),
                        min_put_rate: *min_put_rate,
                        expect_generation: *expect_generation,
                        verify: no_verify.then_some(false),
                    }),
                )
            }
        })
    }
}

/// The body of `PUT /connections/{name}`: the fields the operator gave, and no others (an
/// absent option is absent, not `null`, so the server's default is the server's). It has
/// no `Debug`, so the key cannot reach a log line through it. `kbf_types`' own
/// `ConnectionPut` is the receiving side and has no `Serialize` on purpose; a test parses
/// this body into it, so the two cannot drift.
#[derive(Serialize)]
struct ConnectionBody<'a> {
    endpoint: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<&'a str>,
    access_key_id: &'a str,
    secret_access_key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    conditional_put: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_put_rate: Option<NonZeroU64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expect_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<bool>,
}

/// The secret key of `source`, without its final line ending.
///
/// # Errors
/// The source cannot be read, is not UTF-8, is longer than [`SECRET_READ_MAX`] bytes or
/// holds nothing; or the file is not a regular file only its owner can access.
pub fn read_secret(
    source: &SecretSource,
    stdin: &mut dyn Read,
) -> Result<Zeroizing<String>, CallError> {
    match &source.secret_file {
        Some(path) => {
            let origin = format!("secret file {}", path.display());
            let fail = |why: String| CallError::Secret {
                origin: origin.clone(),
                why,
            };
            // The mode of the file that was opened, not of what the path names now.
            let (mut file, meta) = std::fs::File::open(path)
                .and_then(|file| file.metadata().map(|meta| (file, meta)))
                .map_err(|e| fail(e.to_string()))?;
            if !meta.is_file() {
                return Err(fail("it is not a regular file".to_owned()));
            }
            let mode = meta.permissions().mode() & 0o7777;
            if mode & 0o077 != 0 {
                return Err(fail(format!(
                    "its mode is {mode:04o}, which lets others read it; chmod 600 it"
                )));
            }
            read_text(&mut file, &origin)
        }
        None => read_text(stdin, "standard input"),
    }
}

/// The text of `reader` in a buffer that is zeroed on drop, without its final line ending.
fn read_text(reader: &mut dyn Read, origin: &str) -> Result<Zeroizing<String>, CallError> {
    let fail = |why: &str| CallError::Secret {
        origin: origin.to_owned(),
        why: why.to_owned(),
    };
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .take(SECRET_READ_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(&e.to_string()))?;
    if bytes.len() as u64 > SECRET_READ_MAX {
        return Err(fail("it is longer than a key can be"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| fail("it is not UTF-8"))?;
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    if text.is_empty() {
        return Err(fail("it holds no key"));
    }
    Ok(Zeroizing::new(text.to_owned()))
}

#[cfg(test)]
mod tests;
