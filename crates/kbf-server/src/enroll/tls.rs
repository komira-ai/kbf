//! The enroll listener's certificate (`docs/design/install-and-enrollment.md`, sections 5.2
//! and 5.4): the frontend's own, issued by the CA, so a node that holds only the CA's pin
//! can verify it.
//!
//! The key is made when the listener starts and lives in memory only (it is never written
//! anywhere). The certificate is the `frontend` role's, for the names `--frontend-name`
//! gives, and the chain the listener sends is **the leaf and then the root**: the node
//! looks for the certificate with the pinned hash in the chain it is sent. The certificate
//! lives as long as the CA's lifetime policy says (7 days by default) and is replaced at
//! half its life, under the same key, by [`FrontendTls::keep_fresh`]; new handshakes use
//! the new one (`kbf_tls::TlsSource::replace`) and open connections keep theirs.

use std::time::Duration;

use kbf_ca::{Bundle, CaError, Entropy, FileSigner, PublicKey, Signer as _, SystemEntropy};
use kbf_tls::{TlsError, TlsSource};

use super::ca::Ca;

/// Why the listener's certificate could not be made or replaced.
#[derive(Debug, thiserror::Error)]
pub enum TlsIdentityError {
    /// The CA refused or could not sign.
    #[error("the frontend's certificate: {0}")]
    Ca(#[from] CaError),
    /// The certificate and key do not make a TLS configuration.
    #[error("the frontend's certificate: {0}")]
    Tls(#[from] TlsError),
}

/// How long to wait before trying again after a failed replacement.
const RETRY: Duration = Duration::from_secs(60);

/// The listener's key and the TLS configuration it is served under.
pub struct FrontendTls {
    names: Vec<String>,
    // The listener's TLS key: an ECDSA P-256 key, made with the CA crate's key type and
    // never written anywhere. It signs TLS handshakes only; the CA key is elsewhere.
    key: FileSigner,
    public: PublicKey,
    source: TlsSource,
}

impl FrontendTls {
    /// A new key and a certificate for `names` (DNS names or IP addresses; at least one),
    /// issued at unix time `now`.
    ///
    /// # Errors
    /// [`TlsIdentityError`].
    pub fn start(
        ca: &Ca,
        names: &[String],
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Self, TlsIdentityError> {
        let key = FileSigner::generate()?;
        let public = PublicKey::from_spki(key.public_key()).map_err(|_| CaError::KeyType)?;
        let (cert, secret) = material(ca, names, &key, &public, now, entropy)?;
        let source = TlsSource::fixed("enroll", cert.as_bytes(), secret.as_bytes(), None)?;
        Ok(Self {
            names: names.to_vec(),
            key,
            public,
            source,
        })
    }

    /// The configuration the listener's handshakes use.
    #[must_use]
    pub fn source(&self) -> TlsSource {
        self.source.clone()
    }

    /// Issues a new certificate for the same key at unix time `now` and serves it to new
    /// handshakes.
    ///
    /// # Errors
    /// [`TlsIdentityError`]; the certificate being served is then unchanged.
    pub fn renew(
        &self,
        ca: &Ca,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<(), TlsIdentityError> {
        let (cert, secret) = material(ca, &self.names, &self.key, &self.public, now, entropy)?;
        Ok(self.source.replace(cert.as_bytes(), secret.as_bytes())?)
    }

    /// Replaces the certificate at half its life, for ever: the future never completes. A
    /// failed replacement is logged and tried again in a minute; the old certificate is
    /// served until it expires.
    /// `every` replaces half the lifetime as the interval (a test seam; `None` in service).
    pub async fn keep_fresh(
        self: std::sync::Arc<Self>,
        ca: std::sync::Arc<Ca>,
        every: Option<Duration>,
    ) {
        self.keep_fresh_by(ca, std::sync::Arc::new(unix_now), every)
            .await;
    }

    /// [`Self::keep_fresh`] with the clock the certificates are issued by given.
    async fn keep_fresh_by(
        self: std::sync::Arc<Self>,
        ca: std::sync::Arc<Ca>,
        clock: std::sync::Arc<dyn Fn() -> i64 + Send + Sync>,
        every: Option<Duration>,
    ) {
        let half = every.unwrap_or_else(|| half_life(&ca));
        let mut wait = half;
        loop {
            tokio::time::sleep(wait).await;
            let (this, authority, now) = (
                std::sync::Arc::clone(&self),
                std::sync::Arc::clone(&ca),
                std::sync::Arc::clone(&clock),
            );
            let renewed = tokio::task::spawn_blocking(move || {
                this.renew(&authority, now(), &mut SystemEntropy)
            })
            .await;
            wait = match renewed {
                Ok(Ok(())) => half,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "the enroll listener's certificate was not renewed; trying again");
                    RETRY
                }
                Err(error) => {
                    tracing::warn!(%error, "the enroll listener's certificate task failed; trying again");
                    RETRY
                }
            };
        }
    }
}

/// Half the lifetime the CA gives the frontend's certificate: when it is replaced. (A
/// policy's lifetime is positive, so the absolute value is the lifetime.)
fn half_life(ca: &Ca) -> Duration {
    Duration::from_secs(ca.frontend().policy().lifetime_s().unsigned_abs() / 2)
}

/// The unix time now, from the system clock.
pub(super) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The PEM chain (the new leaf, then the root) and the PEM key for a new certificate.
fn material(
    ca: &Ca,
    names: &[String],
    key: &FileSigner,
    public: &PublicKey,
    now: i64,
    entropy: &mut dyn Entropy,
) -> Result<(String, String), TlsIdentityError> {
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let issued = ca.frontend().issue_frontend(&names, public, now, entropy)?;
    let mut chain = Bundle::new(vec![issued.der])?.to_pem();
    chain.push_str(&ca.bundle_pem());
    Ok((chain, key.to_pem()))
}

#[cfg(test)]
mod tests;
