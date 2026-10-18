//! The allow-list of the remote operator listener: who may present an OIDC token and what
//! its claims must say. Read from the `[remote_operator]` table of a TOML file
//! (`--remote-operator-config`, which may be `kbf.toml` itself). Every value that decides
//! who is let in is required: there is no default issuer, audience, repository or workflow.

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

#[derive(Debug, Deserialize)]
struct File {
    remote_operator: Option<Raw>,
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
    frontend_image_repository: Option<String>,
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
    /// The image repository a frontend update digest is applied to (`registry/path`, no
    /// tag or digest). `None`: the frontend route answers 501.
    pub frontend_image_repository: Option<String>,
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
    /// The allow-list of the `[remote_operator]` table in `text`.
    ///
    /// # Errors
    /// [`ConfigError`]: the text is not TOML, the table is absent or has an unknown key, or
    /// a required value is missing: `issuer` (https, no trailing slash), `audience`,
    /// `repository_id` (digits), `job_workflow_ref` (at least one) and at least one of
    /// `refs` and `environments`.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let file: File = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let raw = file
            .remote_operator
            .ok_or_else(|| ConfigError::Invalid("the [remote_operator] table is absent".into()))?;
        let issuer = need(raw.issuer, "issuer")?;
        if !issuer.starts_with("https://")
            || issuer.ends_with('/')
            || issuer.contains(['?', '#'])
            || issuer.len() <= "https://".len()
        {
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
        let frontend_image_repository = raw
            .frontend_image_repository
            .map(|r| need(Some(r), "frontend_image_repository"))
            .transpose()?;
        if let Some(r) = &frontend_image_repository
            && (r.contains(['@', ' ']) || r.rsplit('/').next().is_some_and(|l| l.contains(':')))
        {
            return Err(ConfigError::Invalid(
                "frontend_image_repository is registry/path with no tag or digest".into(),
            ));
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
            frontend_image_repository,
        })
    }

    /// The allow-list in the file at `path`.
    ///
    /// # Errors
    /// [`ConfigError`]: the file cannot be read, or [`Self::parse`] refuses it.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }
}
