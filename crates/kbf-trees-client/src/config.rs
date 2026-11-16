//! Which node the client calls, with which key, and how the connection is protected.

use std::net::IpAddr;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use http::uri::Uri;

use crate::error::ConfigError;

/// The clock a signature's time (`x-amz-date`) is read from. The node refuses a call signed
/// more than 15 minutes from its own clock.
pub type Clock = fn() -> SystemTime;

/// A node's trees interface: `host:port`, as a host's address label carries it. The host is a
/// DNS name, an IPv4 address, or an IPv6 address in brackets; the port is required.
#[derive(Clone, PartialEq, Eq)]
pub struct Node {
    /// The host as dialled and as TLS names it: without brackets.
    host: String,
    port: u16,
    /// `host:port` as written, which the calls name as their authority and sign as `host`.
    authority: String,
    /// The node under `http`: the address tonic's channel is given (the connector does TLS
    /// itself), and the origin of a call in clear.
    plain: Uri,
    /// The node under `https`, the origin of a call over TLS.
    tls: Uri,
}

impl Node {
    /// The host, without brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// `host:port` as written.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// Whether the host is `localhost` or a loopback address, the only hosts a call may reach
    /// in clear without an opt-in.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }

    /// The URI of the node under `http`, or `https` when `tls`.
    pub(crate) fn uri(&self, tls: bool) -> &Uri {
        if tls { &self.tls } else { &self.plain }
    }
}

/// Prints the address as written: `Node("192.0.2.5:9600")`.
impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Node").field(&self.authority).finish()
    }
}

impl FromStr for Node {
    type Err = ConfigError;

    /// Reads `host:port`: a name or IPv4 address, or `[IPv6]`, then `:` and a port of 1 to
    /// 65535. A scheme, a path, a user, an empty host or a missing port is refused.
    fn from_str(text: &str) -> Result<Self, ConfigError> {
        let bad = |reason| ConfigError::Address {
            address: text.to_owned(),
            reason,
        };
        let parsed = (
            format!("http://{text}").parse::<Uri>(),
            format!("https://{text}").parse::<Uri>(),
        );
        let (Ok(plain), Ok(tls)) = parsed else {
            return Err(bad("not host:port"));
        };
        let authority = plain.authority().map(|a| a.as_str());
        if text.contains('@') || authority != Some(text) {
            return Err(bad("not host:port alone"));
        }
        let Some(port) = plain.port_u16().filter(|port| *port != 0) else {
            // Text after the last `:` that is not inside an IPv6 host's brackets is a port.
            let written = text
                .rsplit_once(':')
                .filter(|(_, port)| !port.contains(']'));
            let why = match written {
                Some(_) => "the port is not 1 to 65535",
                None => "no port",
            };
            return Err(bad(why));
        };
        let host = plain.host().unwrap_or_default();
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if host.is_empty() {
            return Err(bad("no host"));
        }
        // A name is letters, digits, `.`, `-` and `_`; an IPv6 address came in brackets.
        let named = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
        if !text.starts_with('[') && !host.chars().all(named) {
            return Err(bad("the host is not a name or an address"));
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            authority: text.to_owned(),
            plain,
            tls,
        })
    }
}

/// A tierstore access key: its id, and the secret that signs and is never sent or printed.
#[derive(Clone)]
pub struct Key {
    id: String,
    secret: String,
}

impl Key {
    /// The key `id` with `secret`.
    ///
    /// # Errors
    /// [`ConfigError::BadKey`] for an id or secret that is empty or not printable ASCII, or
    /// an id with a space, `,`, `/` or `=` (which the `Authorization` header would misread).
    pub fn new(id: &str, secret: &str) -> Result<Self, ConfigError> {
        let printable = |text: &str| text.chars().all(|c| c.is_ascii_graphic());
        if id.is_empty() || !printable(id) || id.contains([',', '/', '=']) {
            return Err(ConfigError::BadKey("id is empty or not a plain token"));
        }
        if secret.is_empty() || !printable(secret) {
            return Err(ConfigError::BadKey(
                "secret is empty or not printable ASCII",
            ));
        }
        Ok(Self {
            id: id.to_owned(),
            secret: secret.to_owned(),
        })
    }

    /// The key's id, which every signed call names.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }
}

/// Prints the id alone: `Key { id: "AKID", .. }`.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// How a [`Client`](crate::Client) reaches one node.
///
/// A node is called over TLS when a CA is pinned, trusting that CA alone. Without one, a call
/// goes in clear, which is allowed for a loopback node and, off loopback, only when
/// [`Config::allow_plaintext`] says so: the key's secret is never sent (each call is signed
/// with it), but the call itself, its tree and its sources, would be readable and, for 15
/// minutes, replayable on the network.
#[derive(Clone, Debug)]
pub struct Config {
    pub(crate) node: Node,
    pub(crate) key: Key,
    pub(crate) region: String,
    pub(crate) ca_pem: Option<Vec<u8>>,
    pub(crate) allow_plaintext: bool,
    pub(crate) connect_timeout: Duration,
    pub(crate) call_timeout: Duration,
    pub(crate) clock: Clock,
}

impl Config {
    /// The default for the TCP connect and the TLS handshake together.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

    /// The default bound of one call, the design's `--executor-trees-call-timeout-secs`. It is
    /// above the 20 s wait deadline the design asks of tierstore for a `wait` create, so that
    /// the node's answer comes first once tierstore has it; tierstore v0.3.111 waits up to its
    /// `trees.create_timeout` (120 s by default), so until then a cold `wait` create can end
    /// at this client's deadline, as `Transport` (the node may have acted), and is repeated.
    pub const CALL_TIMEOUT: Duration = Duration::from_secs(40);

    /// Calls to `node`, signed with `key` in the scope of `region` (the node's
    /// `server.region`), in clear if the node is on loopback, with the default timeouts and
    /// the system clock.
    ///
    /// # Errors
    /// [`ConfigError::BadRegion`] for a region that is empty, not printable ASCII, or holds
    /// a `/`.
    pub fn new(node: Node, key: Key, region: &str) -> Result<Self, ConfigError> {
        let plain = |c: char| c.is_ascii_graphic() && c != '/';
        if region.is_empty() || !region.chars().all(plain) {
            return Err(ConfigError::BadRegion(region.to_owned()));
        }
        Ok(Self {
            node,
            key,
            region: region.to_owned(),
            ca_pem: None,
            allow_plaintext: false,
            connect_timeout: Self::CONNECT_TIMEOUT,
            call_timeout: Self::CALL_TIMEOUT,
            clock: SystemTime::now,
        })
    }

    /// Calls the node over TLS, trusting the certificates of `pem` alone, and checks that the
    /// node's certificate carries its host.
    #[must_use]
    pub fn with_ca(mut self, pem: Vec<u8>) -> Self {
        self.ca_pem = Some(pem);
        self
    }

    /// Allows calls in clear to a node off loopback when no CA is pinned. The caller logs that
    /// it did so; [`Client::in_clear_off_loopback`](crate::Client::in_clear_off_loopback)
    /// says whether a client does.
    #[must_use]
    pub fn allow_plaintext(mut self) -> Self {
        self.allow_plaintext = true;
        self
    }

    /// The bound of the TCP connect and TLS handshake, and of one call after it.
    #[must_use]
    pub fn with_timeouts(mut self, connect: Duration, call: Duration) -> Self {
        self.connect_timeout = connect;
        self.call_timeout = call;
        self
    }

    /// The clock the signatures read, in place of the system's.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The node.
    #[must_use]
    pub fn node(&self) -> &Node {
        &self.node
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
