//! A streamed GET on a connection: the answer says how long it is, and a body that cannot be
//! completed is cut, so that a client reads less than the length it was told. The server is run
//! over an in-memory duplex pipe, not a socket (hyper's logic for a cut body is the same on both);
//! nothing here waits for a time.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use crate::{Backend, ByteRange, Error, Listed, MemoryBackend, Meta, ReadBody, TestClock};

use super::pipes::{is_cut, raw_get, split_answer};
use super::testing::{BUCKET, KEY_ID, NOW, SECRET};
use super::{Server, ServerConfig};

const SIZE: usize = 1000;

/// How the stream of a read ends.
#[derive(Clone, Copy)]
enum Script {
    /// Every byte, then the end.
    Whole,
    /// Six tenths of the bytes, then the end.
    Short,
    /// Every byte, then an error.
    ErrorAtEnd,
}

/// A backend that holds one object in memory and gives it as chunks of 100 bytes that end as
/// `end` says.
struct Scripted {
    memory: MemoryBackend,
    end: Script,
}

#[async_trait]
impl Backend for Scripted {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.memory.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.memory.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.memory.get(key, range).await
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        let whole = self.memory.get(key, range).await?;
        let kept = match self.end {
            Script::Short => whole.len() * 6 / 10,
            Script::Whole | Script::ErrorAtEnd => whole.len(),
        };
        let mut items: Vec<Result<Bytes, Error>> = whole
            .slice(..kept)
            .chunks(100)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        if matches!(self.end, Script::ErrorAtEnd) {
            items.push(Err(Error::Unavailable("the source failed".to_owned())));
        }
        Ok(stream::iter(items).boxed())
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.memory.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.memory.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.memory.list(prefix).await
    }
}

/// The answer to a GET of the one object of a [`Scripted`] backend that ends as `end` says: the
/// head, the length the head gives, and the body that arrived.
async fn get_with(end: Script) -> Vec<u8> {
    let memory = MemoryBackend::new();
    memory
        .put("obj", Bytes::from(vec![5_u8; SIZE]))
        .await
        .unwrap();
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock.clone());
    config.rate_clock = clock;
    let server = Server::new(Arc::new(Scripted { memory, end }), config.clone());
    raw_get(&server, &config, &format!("/{BUCKET}/obj")).await
}

/// Asserts that a body whose stream is whole is sent with its `Content-Length` and all of its
/// bytes (the check that the three cases below differ from this one only in how the body ends).
#[tokio::test]
async fn a_whole_body_arrives_with_the_length_it_was_told() {
    let answer = get_with(Script::Whole).await;
    let (head, length, body) = split_answer(&answer);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(length, Some(SIZE));
    assert_eq!(body.len(), SIZE);
    assert!(!is_cut(&answer));
}

/// Asserts that a body whose stream ends short is cut: the connection ends before the body of the
/// length the answer gave (the server sends nothing of a head it cannot follow with its body, or
/// the head and fewer bytes than its length), so a client sees the answer as incomplete.
#[tokio::test]
async fn a_body_that_ends_short_ends_the_connection_before_its_length() {
    let answer = get_with(Script::Short).await;
    assert!(is_cut(&answer), "{} bytes", answer.len());
}

/// Asserts that a body whose stream fails after its last byte is cut too: the last chunk is not
/// sent, since a connection that has written the whole length does not ask the body again, so
/// the error would not be heard.
#[tokio::test]
async fn a_body_that_fails_after_its_last_byte_ends_the_connection_before_its_length() {
    let answer = get_with(Script::ErrorAtEnd).await;
    assert!(is_cut(&answer), "{} bytes", answer.len());
}
