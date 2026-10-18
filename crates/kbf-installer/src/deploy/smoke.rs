//! The CAS smoke: after a deploy, one small blob goes through the local REAPI listener
//! the way a client sends it, and the action cache is asked about a digest nobody wrote.
//! It is a client of the listener only (no storage node is named), so it runs against the
//! frontend this deploy just updated.
//!
//! Four checks, in order, the first to fail ends the smoke with a message that names it:
//!
//! 1. `ByteStream.Write` of 64 random bytes (a name no earlier run used) commits as many
//!    bytes as were sent.
//! 2. `FindMissingBlobs` for that blob and for another random digest nobody wrote says
//!    the written one is present and the other missing, and nothing else.
//! 3. `ByteStream.Read` returns exactly the bytes written.
//! 4. `ActionCache.GetActionResult` for a random digest is NOT_FOUND: a hit is a defect
//!    and any other status (the cache down, the service missing) is a failure, not a miss.
//!
//! An `https://` listener is verified against the system roots and an optional extra CA, under
//! a configured certificate name while the connection goes to loopback.
//!
//! The whole run is bounded by one timeout. The blob stays in the store: REAPI has no
//! delete, and 64 bytes of random content is left to the store's eviction.
//! The instance name is empty; a listener that needs credentials is not smoked yet.

use std::io::Read as _;
use std::time::Duration;

use futures::stream;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{Digest, FindMissingBlobsRequest, GetActionResultRequest};
use sha2::{Digest as _, Sha256};
use tonic::Code;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

use super::{Smoke, SmokeResult};

/// The bytes of the blob the smoke writes.
const BLOB_BYTES: usize = 64;

/// The smoke against the REAPI listener at `url` (`http://` for a plain-text listener,
/// `https://` for one that serves TLS).
#[derive(Clone, Debug)]
pub struct CasSmoke {
    url: String,
    timeout: Duration,
    /// The name the server's certificate is verified against (https only).
    server_name: Option<String>,
    /// PEM of CA certificates trusted besides the system's (https only).
    ca_pem: Option<Vec<u8>>,
}

impl CasSmoke {
    /// # Errors
    /// `url` is not an `http://host:port` or `https://host:port` URL.
    pub fn new(url: &str, timeout: Duration) -> Result<CasSmoke, String> {
        let rest = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .unwrap_or_default();
        if rest.is_empty() || rest.contains(['/', '?', '#', ' ']) {
            return Err(format!("{url:?} is not an http(s)://host:port URL"));
        }
        Ok(CasSmoke {
            url: url.to_owned(),
            timeout,
            server_name: None,
            ca_pem: None,
        })
    }

    /// The name the server's certificate must carry (the connection may go to an address
    /// that is not the name: the local listener is dialled on loopback) and PEM of CA
    /// certificates trusted besides the system's roots. Without a name the URL's host is
    /// verified.
    ///
    /// # Errors
    /// The URL is `http://` (the options would do nothing), or `ca_pem` holds no
    /// certificate.
    pub fn tls(
        mut self,
        server_name: Option<String>,
        ca_pem: Option<Vec<u8>>,
    ) -> Result<CasSmoke, String> {
        if self.url.starts_with("http://") && (server_name.is_some() || ca_pem.is_some()) {
            return Err("a certificate name or CA was given for an http:// URL".to_owned());
        }
        if let Some(pem) = &ca_pem
            && !x509_parser::pem::Pem::iter_from_buffer(pem).any(|p| p.is_ok())
        {
            return Err("the CA file holds no PEM certificate".to_owned());
        }
        self.server_name = server_name;
        self.ca_pem = ca_pem;
        Ok(self)
    }
}

impl Smoke for CasSmoke {
    fn run(&self) -> SmokeResult {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => return SmokeResult::Fail(format!("no runtime: {e}")),
        };
        let this = self.clone();
        let bounded = async move { tokio::time::timeout(this.timeout, check(&this)).await };
        match runtime.block_on(bounded) {
            Ok(Ok(())) => SmokeResult::Pass,
            Ok(Err(why)) => SmokeResult::Fail(why),
            Err(_) => SmokeResult::Fail(format!("timed out after {} s", self.timeout.as_secs())),
        }
    }
}

/// `n` bytes from the operating system's random source.
fn random(n: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("no random bytes: {e}"))?;
    Ok(bytes)
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}

async fn check(target: &CasSmoke) -> Result<(), String> {
    let blob = random(BLOB_BYTES)?;
    let digest = digest_of(&blob);
    let absent = digest_of(&random(BLOB_BYTES)?);
    let action = digest_of(&random(BLOB_BYTES)?);
    let upload = hex::encode(random(16)?);

    let mut endpoint =
        Endpoint::from_shared(target.url.clone()).map_err(|e| format!("the address: {e}"))?;
    if target.url.starts_with("https://") {
        // The system roots, the extra CA, and the certificate's own name.
        let mut tls = ClientTlsConfig::new().with_native_roots();
        if let Some(pem) = &target.ca_pem {
            tls = tls.ca_certificate(Certificate::from_pem(pem));
        }
        if let Some(name) = &target.server_name {
            tls = tls.domain_name(name.clone());
        }
        endpoint = endpoint
            .tls_config(tls)
            .map_err(|e| format!("TLS config: {e}"))?;
    }
    let channel: Channel = endpoint
        .connect()
        .await
        .map_err(|e| format!("cannot connect: {e}"))?;

    // 1. Write.
    let write = WriteRequest {
        resource_name: format!(
            "uploads/{upload}/blobs/{}/{}",
            digest.hash, digest.size_bytes
        ),
        write_offset: 0,
        finish_write: true,
        data: blob.clone(),
    };
    let committed = ByteStreamClient::new(channel.clone())
        .write(stream::iter([write]))
        .await
        .map_err(|s| format!("ByteStream.Write: {}", status(&s)))?
        .into_inner()
        .committed_size;
    if committed != digest.size_bytes {
        return Err(format!(
            "ByteStream.Write committed {committed} bytes of {}",
            digest.size_bytes
        ));
    }

    // 2. FindMissingBlobs.
    let missing = ContentAddressableStorageClient::new(channel.clone())
        .find_missing_blobs(FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: vec![digest.clone(), absent.clone()],
            digest_function: 0,
        })
        .await
        .map_err(|s| format!("FindMissingBlobs: {}", status(&s)))?
        .into_inner()
        .missing_blob_digests;
    if missing.iter().any(|d| d.hash == digest.hash) {
        return Err("FindMissingBlobs reports the blob just written as missing".to_owned());
    }
    if !missing.iter().any(|d| d.hash == absent.hash) {
        return Err("FindMissingBlobs reports a blob nobody wrote as present".to_owned());
    }

    // 3. Read it back.
    let mut read = ByteStreamClient::new(channel.clone())
        .read(ReadRequest {
            resource_name: format!("blobs/{}/{}", digest.hash, digest.size_bytes),
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .map_err(|s| format!("ByteStream.Read: {}", status(&s)))?
        .into_inner();
    let mut got = Vec::new();
    while let Some(m) = read
        .message()
        .await
        .map_err(|s| format!("ByteStream.Read: {}", status(&s)))?
    {
        got.extend_from_slice(&m.data);
    }
    if got != blob {
        return Err(format!(
            "ByteStream.Read returned {} bytes that differ from the {} written",
            got.len(),
            blob.len()
        ));
    }

    // 4. A miss in the action cache is NOT_FOUND.
    let reply = ActionCacheClient::new(channel)
        .get_action_result(GetActionResultRequest {
            action_digest: Some(action),
            ..GetActionResultRequest::default()
        })
        .await;
    match reply {
        Err(s) if s.code() == Code::NotFound => Ok(()),
        Err(s) => Err(format!("GetActionResult: {}", status(&s))),
        Ok(_) => Err("GetActionResult hit for a digest nobody wrote".to_owned()),
    }
}

/// A status as one short phrase.
fn status(s: &tonic::Status) -> String {
    format!("{:?}: {}", s.code(), s.message())
}
