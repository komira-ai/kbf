//! `kbf token create|list|revoke`: a thin client of the frontend's enrollment API
//! (`docs/api.md`, `/v1/enroll/tokens`) on the frontend's host. Like `kbf cluster` it holds
//! no logic of its own: each verb is one call with the operator API token, and the answer's
//! JSON is printed as it came, except that a create given `--out` keeps the token string
//! out of the terminal.
//!
//! - `kbf token create --roles daemon[,storage] [--max-uses N] [--expires-in 24h]
//!   [--node-id ID] [--node-prefix P] [--names a,.b] [--replace] [--label L] [--out FILE]`:
//!   `POST /v1/enroll/tokens`. The answer holds the token string, shown once. With `--out`
//!   the string goes to FILE alone, created with `create_new` and mode 0600 before the call
//!   is made (so a file that exists is refused before a token is spent), and FILE is removed
//!   if the call fails, so no empty or partial token file is left; stdout then holds the
//!   token as a list shows it, without the string.
//! - `kbf token list`: `GET /v1/enroll/tokens`.
//! - `kbf token revoke <id>`: `DELETE /v1/enroll/tokens/<id>`.
//!
//! Every call needs `--token-file`, the file the server was started with
//! (`--api-token-file`). The routes are behind the operator gate, so this sends the bearer
//! and `Content-Type: application/json` on every call, a GET and a DELETE included. Exit 0
//! for a 2xx answer, 1 for any other answer or no answer (the error goes to stderr), 2 for a
//! command line that does not parse.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use kbf_server::api_client::{ClientError, OperatorClient, Reply};
use kbf_server::enroll::api::TOKENS;
use kbf_types::TokenId;
use kbf_types::enroll_api::{TokenCreate, TokenCreated};

use crate::cluster::DEFAULT_API;

/// `kbf token`'s command line.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The operator API (the server's `--api-listen`).
    #[arg(long, default_value = DEFAULT_API, global = true)]
    pub api: SocketAddr,
    #[command(subcommand)]
    pub verb: Verb,
}

/// The credential every verb needs.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Auth {
    /// The file holding the server's operator API token.
    #[arg(long, value_name = "FILE")]
    pub token_file: PathBuf,
}

/// What `kbf token` does.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Make a token. The token string is shown once.
    Create(CreateArgs),
    /// List the tokens: never their secrets.
    List {
        #[command(flatten)]
        auth: Auth,
    },
    /// Revoke a token: no new node enrolls with it; enrolled nodes stay.
    Revoke {
        /// The token's id, as `list` shows it.
        id: String,
        #[command(flatten)]
        auth: Auth,
    },
}

/// What `kbf token create` takes.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct CreateArgs {
    /// The roles it grants, comma separated: storage, daemon.
    #[arg(long, value_delimiter = ',', required = true)]
    pub roles: Vec<String>,
    /// How many nodes it can enroll, 1 to 1000.
    #[arg(long, default_value_t = 1)]
    pub max_uses: u32,
    /// How long it lives: a number and s, m, h or d (30m, 24h, 7d). The server's default,
    /// a day, if not given; at most 30 days.
    #[arg(long, value_parser = parse_duration, value_name = "DURATION")]
    pub expires_in: Option<u64>,
    /// The one node id it may enroll.
    #[arg(long)]
    pub node_id: Option<String>,
    /// The prefix every node id it enrolls must carry.
    #[arg(long)]
    pub node_prefix: Option<String>,
    /// Advertise hosts it allows, comma separated: a host, or a suffix starting `.`.
    #[arg(long, value_delimiter = ',')]
    pub names: Vec<String>,
    /// Let it enroll a node id that already exists.
    #[arg(long)]
    pub replace: bool,
    /// A label for you.
    #[arg(long, default_value = "")]
    pub label: String,
    /// Write the token string to this file (created new, mode 0600) instead of printing it.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    #[command(flatten)]
    pub auth: Auth,
}

impl CreateArgs {
    /// The body of the call.
    #[must_use]
    pub fn body(&self) -> TokenCreate {
        TokenCreate {
            roles: self.roles.clone(),
            max_uses: self.max_uses,
            expires_in: self.expires_in,
            node_id: self.node_id.clone(),
            node_prefix: self.node_prefix.clone(),
            names: self.names.clone(),
            replace: self.replace,
            label: self.label.clone(),
        }
    }
}

/// A duration `30s`, `15m`, `24h` or `7d` as seconds.
///
/// # Errors
/// A text that is not a whole number followed by one of `s`, `m`, `h`, `d`.
pub fn parse_duration(text: &str) -> Result<u64, String> {
    let bad = || format!("{text:?} is not a duration like 30m, 24h or 7d");
    let unit = text.chars().last().ok_or_else(bad)?;
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    let number: u64 = text[..text.len() - 1].parse().map_err(|_| bad())?;
    number.checked_mul(scale).ok_or_else(bad)
}

impl Verb {
    fn auth(&self) -> &Auth {
        match self {
            Self::Create(CreateArgs { auth, .. })
            | Self::List { auth }
            | Self::Revoke { auth, .. } => auth,
        }
    }
}

/// Why a verb failed.
#[derive(Debug)]
pub enum TokenError {
    /// The token file, or the connection, or the answer's framing.
    Client(ClientError),
    /// The API answered with a status other than 2xx.
    Refused {
        /// The status.
        status: u16,
        /// The body, which the API never fills with a secret.
        body: String,
    },
    /// The API answered 2xx with something that is not a created token.
    Answer,
    /// The `--out` file could not be created or written.
    Out {
        /// The file.
        path: PathBuf,
        /// What failed.
        source: std::io::Error,
        /// The token that was made but could not be saved, if one was.
        made: Option<String>,
    },
    /// The id given to `revoke` is not a token id.
    Id,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(e) => write!(f, "{e}"),
            Self::Refused { status, body } => write!(f, "{status} {}", body.trim_end()),
            Self::Answer => f.write_str("the API's answer is not a created token"),
            Self::Out { path, source, made } => {
                write!(f, "{}: {source}", path.display())?;
                if let Some(id) = made {
                    write!(
                        f,
                        "; token {id} was made but not saved: revoke it with `kbf token revoke {id}`"
                    )?;
                }
                Ok(())
            }
            Self::Id => f.write_str("a token id is 12 characters of a-z and 2-7"),
        }
    }
}

impl From<ClientError> for TokenError {
    fn from(e: ClientError) -> Self {
        Self::Client(e)
    }
}

/// Runs the verb: the text it prints on stdout (exit 0), else the reason on stderr (exit 1).
#[must_use]
pub fn run(token: &Token) -> ExitCode {
    match execute(token) {
        Ok(text) => {
            let _ = writeln!(std::io::stdout(), "{}", text.trim_end());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kbf token: {e}");
            ExitCode::FAILURE
        }
    }
}

fn success(reply: Reply) -> Result<String, TokenError> {
    if reply.is_success() {
        Ok(reply.body)
    } else {
        Err(TokenError::Refused {
            status: reply.status,
            body: reply.body,
        })
    }
}

/// Makes the verb's call and returns what to print.
///
/// # Errors
/// See [`TokenError`].
pub fn execute(token: &Token) -> Result<String, TokenError> {
    let secret = OperatorClient::read_token(&token.verb.auth().token_file)?;
    let client = OperatorClient::new(token.api, Some(secret));
    match &token.verb {
        Verb::List { .. } => success(client.get_gated(TOKENS)?),
        Verb::Revoke { id, .. } => {
            let id = TokenId::parse(id).map_err(|_| TokenError::Id)?;
            success(client.delete(&format!("{TOKENS}/{id}"))?)
        }
        Verb::Create(args) => {
            let body = serde_json::to_string(&args.body()).unwrap_or_default();
            match &args.out {
                None => success(client.post(TOKENS, &body)?),
                Some(path) => create_into(&client, &body, path),
            }
        }
    }
}

/// The file `path`, new, readable and writable by its owner alone from the moment it
/// exists. A file that exists (a symlink included) is refused.
fn open_new(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Writes `line` and a newline to `out`.
fn save(out: &mut impl Write, line: &str) -> std::io::Result<()> {
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// The call of a create and its answer, the token string written to `out`.
fn deliver(
    client: &OperatorClient,
    body: &str,
    path: &Path,
    out: &mut impl Write,
) -> Result<TokenCreated, TokenError> {
    let text = success(client.post(TOKENS, body)?)?;
    let created: TokenCreated = serde_json::from_str(&text).map_err(|_| TokenError::Answer)?;
    save(out, &created.token).map_err(|source| TokenError::Out {
        path: path.to_owned(),
        source,
        made: Some(created.info.id.clone()),
    })?;
    Ok(created)
}

/// A create whose token string goes to the file at `path` and nowhere else. The file is
/// made first, so a path that cannot be written is found before a token is spent, and is
/// removed again when anything after that fails.
fn create_into(client: &OperatorClient, body: &str, path: &Path) -> Result<String, TokenError> {
    let mut file = open_new(path).map_err(|source| TokenError::Out {
        path: path.to_owned(),
        source,
        made: None,
    })?;
    let done = deliver(client, body, path, &mut file);
    drop(file);
    match done {
        Ok(created) => Ok(serde_json::to_string(&created.info).unwrap_or_default()),
        Err(e) => {
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests;
