//! The host's deploy config (`/etc/kbf-installer/deploy.json`): the one type `deploy` reads and
//! `setup` writes, so the two cannot drift. It holds paths and names only, never a token or a
//! certificate: the files themselves are the operator's to place.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where the host says which operator API to call and where its token is, so a caller that may
/// only run `deploy` names neither.
pub const CONFIG: &str = "/etc/kbf-installer/deploy.json";

/// The config. Every key is optional to the reader (a flag can give it instead); `setup` owns
/// the first five and keeps whatever else the file holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfig {
    /// The proxy's public port on this host (loopback), which serves every call of the deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// CA certificates (PEM) the API's certificate is verified against, besides the system's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_ca: Option<PathBuf>,
    /// The name the API's certificate carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_server_name: Option<String>,
    /// The operator token's file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<PathBuf>,
    /// `gamma` or `prod`: the stage this host is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// The local REAPI listener the CAS smoke uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cas: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cas_ca: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cas_server_name: Option<String>,
    /// Keys this version does not know: kept when `setup` rewrites the file.
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_json::Value>,
}

impl HostConfig {
    /// Read the file's text. The error names the place in the text, never a value: a config
    /// that is wrong may hold a path or name the log should not keep.
    pub fn parse(text: &str) -> Result<HostConfig, String> {
        serde_json::from_str(text).map_err(|e| {
            format!(
                "is not a JSON object of the config's keys (line {}, column {})",
                e.line(),
                e.column()
            )
        })
    }

    /// The file's text: keys in a fixed order, one per line, a newline at the end.
    pub fn render(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a config is JSON");
        text.push('\n');
        text
    }
}
