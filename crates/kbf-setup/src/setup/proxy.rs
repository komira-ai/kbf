//! The proxy's flags of `kbf-installer setup`, which `setup` takes only to refuse.
//!
//! `--proxy` once set a host up behind the proxy's two-slot supervision: `setup` wrote the slot
//! table (`[proxy.upstream]`) of `installer.toml` and enabled the proxy's units in place of
//! `kbf-frontend.service`. That supervision was removed. A proxy now balances frontends that run
//! under their own units, configured by hand with one `[[proxy.frontend]]` table for each in the
//! `[proxy]` table of `installer.toml` (docs/design/kbf-proxy-api.md, the pool section), so
//! `setup` writes no proxy table and enables no proxy unit.
//!
//! The flags stay defined, hidden from the help, so that a script written for them gets this
//! reason from [`ProxyFlags::refusal`] (exit 2, nothing read or changed) and not a parser's
//! "unexpected argument". A `[proxy]` table that `installer.toml` already holds is not dropped or
//! changed: it is read by [`crate::installer_toml::InstallerToml`] and written back with every key
//! kept, whatever flags `setup` is run with.

use super::SetupError;

/// What replaced the removed two-slot supervision, which the refusal of the proxy's flags carries.
const POOL_BY_HAND: &str = "a proxy now balances frontends that run under their own units, configured by hand with a [[proxy.frontend]] table for each frontend in the [proxy] table of installer.toml (docs/design/kbf-proxy-api.md, the pool section)";

/// The flags as the command line gave them. Every one is hidden and refused.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyFlags {
    /// Refused: setup no longer sets a host up behind the proxy.
    #[arg(long, hide = true)]
    pub proxy: bool,
    /// Refused with --proxy: the first port of the removed two-slot supervision.
    #[arg(long, hide = true)]
    pub proxy_base_port: Option<u16>,
    /// Refused with --proxy: how callers authenticated to the proxy that setup wrote.
    #[arg(long, hide = true)]
    pub buck2_auth: Option<String>,
    /// Refused with --proxy: the Buck2 API key file of the proxy that setup wrote.
    #[arg(long, hide = true)]
    pub buck2_token_file: Option<String>,
    /// Refused with --proxy: the sibling port of the removed all-in-one layout.
    #[arg(long, hide = true)]
    pub storage_port: Option<u16>,
}

impl ProxyFlags {
    /// The flags that were given, as the command line spells them, in the order they are declared.
    pub fn given(&self) -> Vec<&'static str> {
        [
            (self.proxy, "--proxy"),
            (self.proxy_base_port.is_some(), "--proxy-base-port"),
            (self.buck2_auth.is_some(), "--buck2-auth"),
            (self.buck2_token_file.is_some(), "--buck2-token-file"),
            (self.storage_port.is_some(), "--storage-port"),
        ]
        .into_iter()
        .filter_map(|(given, flag)| given.then_some(flag))
        .collect()
    }

    /// The refusal of the flags that were given, naming each and the reason (exit 2); none when
    /// none was given. It is made from the flags alone, so a run that is refused has read
    /// nothing from the host and changed nothing.
    pub fn refusal(&self) -> Option<SetupError> {
        let given = self.given();
        if given.is_empty() {
            return None;
        }
        let flags = given.join(", ");
        let message = format!(
            "{flags}: two-slot supervision was removed, so setup no longer sets up a proxy host: {POOL_BY_HAND}"
        );
        Some(SetupError::Invalid(vec![message]))
    }
}
