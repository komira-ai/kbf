//! The socket between the unprivileged agent and the root part (docs/design/host-updates.md,
//! 3.4): a unix socket of mode 0600 owned by the agent's account, which is the whole access
//! control, as the helper's is (`kbf-host-install --serve`). One JSON line in, one JSON line out,
//! one thread per connection, so a step that runs for an hour holds up no other request.
//!
//! The requests take a service and a version and nothing else: no command, no path, no rule
//! text and no step. The root part chooses the step from its record and the table of the
//! contract (3.2); an agent that could name the step could run `cleanup` after a failed `verify`.
//!
//! ```json
//! {"update": {"service": "tierstore", "version": "2.0.0"}}
//! {"status": {"service": "tierstore"}}
//! ```
//!
//! An answer is an [`Answer`]: `ok`, and the verdict of the update, or the step the record is at,
//! or an `error` token.

use std::fs::{self, DirBuilder};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, chown};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use serde::{Deserialize, Serialize};
use update_hook::{Step, Verdict};

/// The longest request line.
pub const MAX_LINE: u64 = 4096;

/// What the agent asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Run the update of `service` to the staged release `version` to its verdict, or resume it.
    Update { service: String, version: String },
    /// Where the last update of `service` is.
    Status { service: String },
}

/// What the root part answers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub ok: bool,
    /// Why the request was refused or failed: a token, then `: ` and a text for the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How the update ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The hook's reason token, or the updater's (`rights_change`, `artifact_changed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For `rights_change`: what `approve <service> <digest>` takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// For `rights_change`: the fields that ask for more.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widened: Vec<String>,
    /// For `status`: the step the record is at, and its state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// For the start pass: how the boot entry ended (`success`, `failure`, `deferred`, `halt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<String>,
}

impl Answer {
    /// A refusal: `token`, and what it was about.
    pub fn refused(token: &str, why: impl std::fmt::Display) -> Answer {
        Answer {
            error: Some(format!("{token}: {why}")),
            ..Answer::default()
        }
    }

    /// The token of a refusal.
    pub fn token(&self) -> Option<&str> {
        self.error
            .as_deref()
            .map(|error| error.split(':').next().unwrap_or_default())
    }
}

/// Binds the socket at `path`, mode 0600, owned by `uid` and `gid`. The socket is made in a
/// private directory beside it (mode 0700) and given its owner and mode there, then moved into
/// place, so nobody can connect between the bind and the chown. A socket file already at `path`
/// is replaced.
///
/// # Errors
/// What the file system says.
pub fn bind(path: &Path, uid: u32, gid: u32) -> io::Result<UnixListener> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let private = dir.join(format!(".bind-{}", std::process::id()));
    let _ = fs::remove_dir_all(&private);
    DirBuilder::new().mode(0o700).create(&private)?;
    let made = private.join("s");
    let listener = UnixListener::bind(&made)?;
    chown(&made, Some(uid), Some(gid))?;
    fs::set_permissions(&made, fs::Permissions::from_mode(0o600))?;
    fs::rename(&made, path)?;
    fs::remove_dir(&private)?;
    Ok(listener)
}

/// Reads one request from `stream` and writes the answer `answer` gives.
///
/// # Errors
/// The connection failed.
pub fn serve_connection(
    stream: &UnixStream,
    answer: &dyn Fn(&Request) -> Answer,
) -> io::Result<()> {
    let mut line = String::new();
    BufReader::new(stream.take(MAX_LINE)).read_line(&mut line)?;
    let reply = serde_json::from_str::<Request>(line.trim_end()).map_or_else(
        |e| Answer::refused("malformed", e),
        |request| answer(&request),
    );
    write_line(stream, &reply)
}

fn write_line<T: Serialize>(mut stream: &UnixStream, value: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)
}

/// The agent's side: sends `request` to the root part at `socket` and reads the answer.
///
/// # Errors
/// The socket could not be reached, or the answer is not one.
pub fn request(socket: &Path, request: &Request) -> io::Result<Answer> {
    let stream = UnixStream::connect(socket)?;
    write_line(&stream, request)?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    serde_json::from_str(line.trim_end()).map_err(io::Error::other)
}
