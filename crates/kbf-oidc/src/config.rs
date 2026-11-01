//! The allow-list of the remote operator listener: who may present an OIDC token and what
//! its claims must say. Read from the `[remote_operator]` table of a TOML file
//! (`--remote-operator-config`, which may be `kbf.toml` itself), or from several
//! `[[remote_operator]]` tables, one entry each; a file never has both forms. Every value
//! that decides who is let in is required: there is no default issuer, audience,
//! repository or workflow.

use std::path::Path;

use serde::Deserialize;

/// The most a token's `exp - iat` may be, unless the file lowers or raises it.
pub const DEFAULT_MAX_LIFETIME_SECS: u64 = 600;

/// The most the file may raise the limit to.
pub const MAX_LIFETIME_CEILING_SECS: u64 = 3600;

/// How far `exp`, `nbf` and `iat` may be off this host's clock.
pub const LEEWAY_SECS: u64 = 60;

/// Why the allow-list is refused.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file cannot be read.
    #[error("read {path}: {source}")]
    Read {
        /// The file.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// The text is not TOML this table can come from.
    #[error("remote_operator: {0}")]
    Parse(String),
    /// A value is missing or unusable.
    #[error("remote_operator: {0}")]
    Invalid(String),
}

/// The services an entry may set when it names none.
pub const DEFAULT_SERVICES: &str = "kbf";

#[derive(Debug, Deserialize)]
struct File {
    remote_operator: Option<toml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    issuer: Option<String>,
    audience: Option<String>,
    repository_id: Option<String>,
    repository: Option<String>,
    #[serde(default)]
    job_workflow_ref: Vec<String>,
    #[serde(default)]
    refs: Vec<String>,
    #[serde(default)]
    environments: Vec<String>,
    event_names: Option<Vec<String>>,
    runner_environment: Option<String>,
    max_token_lifetime_secs: Option<u64>,
    services: Option<Vec<String>>,
}

/// What a token's claims must equal. Built only by [`OidcConfig::parse`], so a value of
/// this type has every required field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcConfig {
    /// The token's `iss`, exactly; also where the keys are discovered.
    pub issuer: String,
    /// The token's `aud`, exactly.
    pub audience: String,
    /// The token's `repository_id`.
    pub repository_id: String,
    /// The token's `repository`, if the file names it too.
    pub repository: Option<String>,
    /// The token's `job_workflow_ref`: one of these, exactly (each ends in `@<ref>`).
    pub job_workflow_refs: Vec<String>,
    /// The token's `ref`, if the file lists any: one of these.
    pub refs: Vec<String>,
    /// The token's `environment`, if the file lists any: one of these. When both this and
    /// [`Self::refs`] are listed, both must match.
    pub environments: Vec<String>,
    /// The token's `event_name`: one of these (`push` and `workflow_dispatch` by default).
    pub event_names: Vec<String>,
    /// The token's `runner_environment` (`github-hosted` by default).
    pub runner_environment: String,
    /// The most `exp - iat` may be.
    pub max_lifetime_secs: u64,
    /// The services a token of this entry may set the desired version of (`["kbf"]` when
    /// the file names none).
    pub services: Vec<String>,
}

fn need(value: Option<String>, name: &str) -> Result<String, ConfigError> {
    match value {
        Some(v) if !v.trim().is_empty() && v.trim() == v => Ok(v),
        Some(_) => Err(ConfigError::Invalid(format!(
            "{name} is empty or has surrounding whitespace"
        ))),
        None => Err(ConfigError::Invalid(format!("{name} is required"))),
    }
}

fn clean_list(list: Vec<String>, name: &str) -> Result<Vec<String>, ConfigError> {
    if list.iter().any(|v| v.is_empty() || v.trim() != v) {
        return Err(ConfigError::Invalid(format!(
            "{name} holds an empty entry or one with surrounding whitespace"
        )));
    }
    Ok(list)
}

impl OidcConfig {
    /// The allow-list of the one `[remote_operator]` table in `text`.
    ///
    /// # Errors
    /// [`ConfigError`]: the text is not TOML, the table is absent or has an unknown key, or
    /// a required value is missing: `issuer` (https, no trailing slash), `audience`,
    /// `repository_id` (digits), `job_workflow_ref` (at least one) and at least one of
    /// `refs` and `environments`. Several `[[remote_operator]]` entries are refused here:
    /// read them with [`AllowList::parse`].
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut entries = entries_of(text)?;
        match (entries.pop(), entries.is_empty()) {
            (Some(one), true) => Ok(one),
            _ => Err(ConfigError::Invalid(
                "several [[remote_operator]] entries: read the file as an allow-list".into(),
            )),
        }
    }

    /// The allow-list in the file at `path`.
    ///
    /// # Errors
    /// [`ConfigError`]: the file cannot be read, or [`Self::parse`] refuses it.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&read(path)?)
    }

    fn from_raw(raw: Raw) -> Result<Self, ConfigError> {
        let issuer = need(raw.issuer, "issuer")?;
        if !issuer.starts_with("https://") || issuer.ends_with('/') || issuer.contains(['?', '#']) {
            return Err(ConfigError::Invalid(
                "issuer must be an https:// URL with no trailing slash, query or fragment".into(),
            ));
        }
        let audience = need(raw.audience, "audience")?;
        let repository_id = need(raw.repository_id, "repository_id")?;
        if !repository_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ConfigError::Invalid(
                "repository_id is the repository's numeric id, as a string of digits".into(),
            ));
        }
        let repository = raw
            .repository
            .map(|r| need(Some(r), "repository"))
            .transpose()?;
        let job_workflow_refs = clean_list(raw.job_workflow_ref, "job_workflow_ref")?;
        if job_workflow_refs.is_empty() {
            return Err(ConfigError::Invalid("job_workflow_ref is required".into()));
        }
        let refs = clean_list(raw.refs, "refs")?;
        let environments = clean_list(raw.environments, "environments")?;
        if refs.is_empty() && environments.is_empty() {
            return Err(ConfigError::Invalid(
                "refs or environments is required: name the branch or the protected \
                 environment a deploy runs from"
                    .into(),
            ));
        }
        let event_names = clean_list(
            raw.event_names
                .unwrap_or_else(|| vec!["push".to_owned(), "workflow_dispatch".to_owned()]),
            "event_names",
        )?;
        if event_names.is_empty() || event_names.iter().any(|e| e.starts_with("pull_request")) {
            return Err(ConfigError::Invalid(
                "event_names must be non-empty and never a pull_request event".into(),
            ));
        }
        let max_lifetime_secs = raw
            .max_token_lifetime_secs
            .unwrap_or(DEFAULT_MAX_LIFETIME_SECS);
        if max_lifetime_secs == 0 || max_lifetime_secs > MAX_LIFETIME_CEILING_SECS {
            return Err(ConfigError::Invalid(format!(
                "max_token_lifetime_secs is 1 to {MAX_LIFETIME_CEILING_SECS}"
            )));
        }
        let services = match raw.services {
            None => vec![DEFAULT_SERVICES.to_owned()],
            Some(list) => clean_list(list, "services")?,
        };
        if services.is_empty() {
            return Err(ConfigError::Invalid("services must not be empty".into()));
        }
        Ok(Self {
            issuer,
            audience,
            repository_id,
            repository,
            job_workflow_refs,
            refs,
            environments,
            event_names,
            runner_environment: need(
                Some(
                    raw.runner_environment
                        .unwrap_or_else(|| "github-hosted".to_owned()),
                ),
                "runner_environment",
            )?,
            max_lifetime_secs,
            services,
        })
    }
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })
}

/// The entries of `text`: the one `[remote_operator]` table, or each `[[remote_operator]]`.
fn entries_of(text: &str) -> Result<Vec<OidcConfig>, ConfigError> {
    let file: File = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let raws = match file.remote_operator {
        None => {
            return Err(ConfigError::Invalid(
                "the [remote_operator] table is absent".into(),
            ));
        }
        Some(table @ toml::Value::Table(_)) => vec![table],
        Some(toml::Value::Array(list)) => list,
        Some(_) => {
            return Err(ConfigError::Invalid(
                "remote_operator is a table or an array of tables".into(),
            ));
        }
    };
    raws.into_iter()
        .map(|v| {
            let raw = v
                .try_into::<Raw>()
                .map_err(|e| ConfigError::Parse(e.to_string()))?;
            OidcConfig::from_raw(raw)
        })
        .collect()
}

/// Every entry of the allow-list: a token is admitted by the entry it matches, and by no
/// other. Built only by [`AllowList::parse`] or from one [`OidcConfig`], so it holds at
/// least one entry, all of one issuer (the keys are the issuer's), and no two entries for
/// the same repository and workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowList {
    entries: Vec<OidcConfig>,
}

impl From<OidcConfig> for AllowList {
    fn from(entry: OidcConfig) -> Self {
        Self {
            entries: vec![entry],
        }
    }
}

impl AllowList {
    /// The allow-list of `text`: one `[remote_operator]` table, or several
    /// `[[remote_operator]]` entries.
    ///
    /// # Errors
    /// [`ConfigError`]: [`OidcConfig::parse`]'s reasons for any entry; the file has
    /// both forms (TOML itself refuses that) or an empty list; entries name different
    /// issuers; or two entries share a `repository_id` and a `job_workflow_ref`.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let entries = entries_of(text)?;
        let Some(first) = entries.first() else {
            return Err(ConfigError::Invalid(
                "the [[remote_operator]] list is empty".into(),
            ));
        };
        if entries.iter().any(|e| e.issuer != first.issuer) {
            return Err(ConfigError::Invalid(
                "entries name different issuers: the keys are one issuer's".into(),
            ));
        }
        for (i, a) in entries.iter().enumerate() {
            for b in &entries[i + 1..] {
                if a.repository_id == b.repository_id
                    && a.job_workflow_refs
                        .iter()
                        .any(|w| b.job_workflow_refs.contains(w))
                {
                    return Err(ConfigError::Invalid(format!(
                        "two entries for repository_id {} share a job_workflow_ref",
                        a.repository_id
                    )));
                }
            }
        }
        Ok(Self { entries })
    }

    /// The allow-list in the file at `path`.
    ///
    /// # Errors
    /// [`ConfigError`]: the file cannot be read, or [`Self::parse`] refuses it.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&read(path)?)
    }

    /// The entries, in file order.
    #[must_use]
    pub fn entries(&self) -> &[OidcConfig] {
        &self.entries
    }

    pub(crate) fn issuer(&self) -> &str {
        self.entries.first().map_or("", |e| e.issuer.as_str())
    }
}
