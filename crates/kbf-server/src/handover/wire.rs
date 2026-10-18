//! The handover's wire: one line per message on the control socket.
//!
//! ```text
//! successor -> holder   hello <version> <instance a|b|-> <reapi> <state path, hex>
//! holder -> successor   offer <generation>
//!                       refused <why>
//! successor -> holder   ready
//! holder -> successor   released                           (or: failed <why>)
//! successor -> holder   serving
//! ```
//!
//! No socket travels with a message: the successor binds its own.

use std::ffi::OsString;
use std::io;
use std::net::SocketAddr;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

use super::Instance;

/// The protocol version a hello names.
pub const PROTOCOL: u32 = 2;

/// One message of the handover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// The successor asks to take over: its protocol, the instance it is, the REAPI
    /// address it bound (where the holder forwards the calls that still reach it) and its
    /// state file.
    Hello {
        version: u32,
        instance: Option<Instance>,
        reapi: SocketAddr,
        state: PathBuf,
    },
    /// The holder agrees, and says the generation the successor must find the state
    /// store at.
    Offer { generation: u64 },
    /// The holder will not hand over.
    Refused(String),
    /// The successor has bound its listeners and built all it can without the state
    /// store: the holder may stand down.
    Ready,
    /// The holder has stopped, and given up the state store.
    Released,
    /// The holder could not stop cleanly.
    Failed(String),
    /// The successor serves and is ready: the holder may leave Envoy's rotation once
    /// Envoy has seen it.
    Serving,
}

impl Message {
    /// The message as a line, without its newline.
    pub fn line(&self) -> String {
        match self {
            Self::Hello {
                version,
                instance,
                reapi,
                state,
            } => {
                let instance = instance.map_or('-', Instance::letter);
                let state = hex::encode(state.as_os_str().as_bytes());
                format!("hello {version} {instance} {reapi} {state}")
            }
            Self::Offer { generation } => format!("offer {generation}"),
            Self::Refused(why) => format!("refused {}", one_line(why)),
            Self::Ready => "ready".to_owned(),
            Self::Released => "released".to_owned(),
            Self::Failed(why) => format!("failed {}", one_line(why)),
            Self::Serving => "serving".to_owned(),
        }
    }

    /// The message a line is, if it is one.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        match verb {
            "hello" => hello(rest),
            "offer" => Some(Self::Offer {
                generation: rest.parse().ok()?,
            }),
            "refused" => Some(Self::Refused(rest.to_owned())),
            "ready" => Some(Self::Ready),
            "released" => Some(Self::Released),
            "failed" => Some(Self::Failed(rest.to_owned())),
            "serving" => Some(Self::Serving),
            _ => None,
        }
    }
}

/// `text` with its line breaks made spaces, so it stays one line.
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

fn hello(rest: &str) -> Option<Message> {
    let mut fields = rest.split(' ');
    let version = fields.next()?.parse().ok()?;
    let instance = match fields.next()? {
        "-" => None,
        letter => Some(letter.parse().ok()?),
    };
    let reapi = fields.next()?.parse().ok()?;
    let state = PathBuf::from(OsString::from_vec(hex::decode(fields.next()?).ok()?));
    Some(Message::Hello {
        version,
        instance,
        reapi,
        state,
    })
}

/// Why the control connection failed.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The socket failed.
    #[error("control socket: {0}")]
    Io(#[from] io::Error),
    /// The other end closed the connection.
    #[error("the other process closed the control connection")]
    Closed,
    /// The other end sent something that is not a message.
    #[error("not a handover message: {0:?}")]
    Malformed(String),
}

/// One end of a control connection.
pub struct Link {
    stream: BufReader<UnixStream>,
}

impl Link {
    /// The connection `stream`.
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream: BufReader::new(stream),
        }
    }

    /// Sends `message`.
    ///
    /// # Errors
    /// The socket fails.
    pub async fn say(&mut self, message: &Message) -> io::Result<()> {
        let line = format!("{}\n", message.line());
        self.stream.get_mut().write_all(line.as_bytes()).await
    }

    /// The next message.
    ///
    /// # Errors
    /// The socket fails, the other end has closed it, or what it sent is not a message.
    pub async fn hear(&mut self) -> Result<Message, WireError> {
        let mut line = String::new();
        let read = self.stream.read_line(&mut line).await?;
        if read == 0 {
            return Err(WireError::Closed);
        }
        Message::parse(&line).ok_or_else(|| WireError::Malformed(line.trim_end().to_owned()))
    }
}

#[cfg(test)]
impl Link {
    /// Writes `bytes` as they are, for a test that sends what is not a message.
    pub(crate) async fn say_raw(&mut self, bytes: &[u8]) {
        self.stream.get_mut().write_all(bytes).await.expect("write");
    }
}

#[cfg(test)]
mod tests;
