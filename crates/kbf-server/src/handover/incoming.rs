//! The frontend that starts while another serves, when it asks to take over (see [`super`]).

use std::path::Path;

use tokio::net::UnixStream;

use super::HandoverError;
use super::wire::{Link, Message};

/// What the serving frontend offered.
pub(crate) struct Offer {
    /// The control connection, to say `ready` on and to hear `released` on.
    link: Link,
    /// The generation the state store must be found at.
    pub generation: u64,
}

/// Asks the frontend serving on `socket` to hand over, saying what this process is
/// started with (`hello`). `None` if nothing serves there (no socket file, or one nobody
/// listens on): this is the only frontend and starts as any start does.
///
/// # Errors
/// The socket cannot be used, or the frontend refuses or answers wrongly.
pub(crate) async fn knock(socket: &Path, hello: &Message) -> Result<Option<Offer>, HandoverError> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let mut link = Link::new(stream);
    link.say(hello).await?;
    match link.hear().await? {
        Message::Offer { generation } => Ok(Some(Offer { link, generation })),
        Message::Refused(why) => Err(HandoverError::Refused(why)),
        other => Err(HandoverError::Unexpected(other.line())),
    }
}

impl Offer {
    /// Says this process has bound its listeners and accepts the offer, and waits for the
    /// frontend serving to stand down.
    ///
    /// # Errors
    /// The frontend fails to stand down, or the connection fails.
    pub async fn ready(mut self) -> Result<Released, HandoverError> {
        self.link.say(&Message::Ready).await?;
        match self.link.hear().await? {
            Message::Released => Ok(Released(self.link)),
            Message::Failed(why) => Err(HandoverError::Failed(why)),
            other => Err(HandoverError::Unexpected(other.line())),
        }
    }
}

/// The control connection of a successor whose predecessor has released: the way to tell
/// it the successor serves.
pub(crate) struct Released(Link);

impl Released {
    /// Tells the predecessor this process serves and is ready, so it may leave Envoy's
    /// rotation.
    ///
    /// # Errors
    /// The connection fails (the predecessor is gone, and goes not ready on its own).
    pub async fn serving(mut self) -> std::io::Result<()> {
        self.0.say(&Message::Serving).await
    }
}

#[cfg(test)]
impl Released {
    /// The connection `link`, for a test that plays the successor's side.
    pub(crate) fn over(link: Link) -> Self {
        Self(link)
    }
}

#[cfg(test)]
mod tests;
