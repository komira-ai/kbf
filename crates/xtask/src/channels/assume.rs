//! `channels assume`: the job's AWS credentials from the OIDC token of the run, without a
//! third-party action (the repository's Actions policy admits GitHub's own actions and a short
//! list of others, and `aws-actions/configure-aws-credentials` is not on it).
//!
//! 1. The runner offers a token request (`ACTIONS_ID_TOKEN_REQUEST_URL`, authorised by
//!    `ACTIONS_ID_TOKEN_REQUEST_TOKEN`) to a job that holds `id-token: write`. The workflow
//!    passes the URL as `--token-url`; the bearer, a secret, is the only environment this verb
//!    reads ([`BEARER_VAR`]).
//! 2. The token is asked for with the audience `sts.amazonaws.com` over HTTPS.
//! 3. `aws sts assume-role-with-web-identity` exchanges it for the role's credentials. The token
//!    is never an argument (arguments are readable by every process of the machine, in
//!    `/proc/<pid>/cmdline`): it is written to a file of mode 0600 in a private directory (0700)
//!    made below [`Request::token_dir`], and passed as `--web-identity-token file://<path>`; the
//!    file and the directory are removed when the exchange is over, whatever it came to.
//! 4. `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` and the region are
//!    appended to the env file (`GITHUB_ENV`), so the steps after this one carry them.
//!
//! Every secret value is masked (`::add-mask::`) on the log before it is used for anything else,
//! and none is ever printed or put in an error.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::Error;
use super::aws::Aws;
use crate::fetch::{host_roots, tls_config};

/// The audience the AWS STS trusts a GitHub token for.
pub const AUDIENCE: &str = "sts.amazonaws.com";

/// How long the credentials last, in seconds (the shortest AWS allows for a role chain is 900;
/// the most a release job needs is far less than this).
const DURATION_SECONDS: &str = "3600";

/// How long the token request may take.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(60);

/// The most the token answer may be.
const MAX_TOKEN_ANSWER: u64 = 1024 * 1024;

/// The environment variable that holds the bearer of the token request, set by the runner.
pub const BEARER_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_TOKEN";

/// The runner's token service: where to ask and the bearer that authorises it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenService {
    /// The request URL the runner offers.
    pub url: String,
    /// The bearer of the request: a secret.
    pub key: String,
}

/// The token request of this job from its URL and bearer.
///
/// # Errors
/// The runner offers none (an empty URL or no bearer): the job lacks `id-token: write`.
pub fn token_service(url: &str, key: Option<String>) -> Result<TokenService, Error> {
    match key.filter(|k| !k.is_empty()) {
        Some(key) if !url.is_empty() => Ok(TokenService {
            url: url.to_owned(),
            key,
        }),
        _ => Err(Error::Failed(
            "the runner offers no OIDC token (the job needs `permissions: id-token: write`)"
                .to_owned(),
        )),
    }
}

/// The token request of this job: `url` and the bearer in the environment.
///
/// # Errors
/// The runner offers none: the job lacks `id-token: write`.
pub fn oidc_from_env(url: &str) -> Result<TokenService, Error> {
    token_service(url, std::env::var(BEARER_VAR).ok())
}

/// What `assume` is asked for.
pub struct Request<'a> {
    /// The role to assume.
    pub role_arn: &'a str,
    /// The session name AWS records.
    pub session_name: &'a str,
    /// The region the later steps use.
    pub region: &'a str,
    /// The file the `AWS_*` lines are appended to (`GITHUB_ENV`).
    pub env_file: &'a Path,
    /// The directory below which the private directory of the token file is made (it must
    /// exist; the directory of the token is named for the process and must not).
    pub token_dir: &'a Path,
    /// Whether the token request must be HTTPS (it always is, bar the tests of the request on the
    /// loopback address).
    pub https_only: bool,
}

/// A value that may be written as one `NAME=value` line of an env file: not empty, one line, no
/// control character.
fn plain(what: &str, value: &str) -> Result<(), Error> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(Error::Usage(format!(
            "{what} is empty or has a control character"
        )));
    }
    Ok(())
}

/// A session name as AWS takes it: letters, digits and `_+=,.@-`, up to 64.
fn session_ok(name: &str) -> bool {
    (2..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_+=,.@-".contains(&b))
}

/// The token in a file of its own, in a private directory; both are removed when this is dropped.
struct TokenFile {
    dir: PathBuf,
    path: PathBuf,
}

impl TokenFile {
    /// Writes `jwt` to `<parent>/assume-<pid>/token`: the directory is made with mode 0700 and
    /// must not exist, the file with mode 0600 from its first byte.
    fn write(parent: &Path, jwt: &str) -> Result<TokenFile, Error> {
        let dir = parent.join(format!("assume-{}", std::process::id()));
        let file = TokenFile {
            path: dir.join("token"),
            dir,
        };
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&file.dir)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&file.path)
            })
            .and_then(|mut f| f.write_all(jwt.as_bytes()))
            .map_err(Error::spawn(format!(
                "write the token file {}",
                file.path.display()
            )))?;
        Ok(file)
    }

    /// The value of `--web-identity-token` that reads the token from the file.
    fn argument(&self) -> String {
        format!("file://{}", self.path.display())
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        // The file is ours alone; a directory that was there before is left (not empty) or
        // removed (empty), and a file that is gone already is no failure.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Asks the runner for the token: the JWT.
fn request_token(oidc: &TokenService, https_only: bool) -> Result<String, Error> {
    let url = format!(
        "{}{}audience={AUDIENCE}",
        oidc.url,
        if oidc.url.contains('?') { '&' } else { '?' }
    );
    // An error here never repeats the request: its url and bearer are the runner's.
    let failed = |why: String| Error::Failed(format!("the OIDC token request failed: {why}"));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(https_only)
        .timeout_global(Some(TOKEN_TIMEOUT))
        .tls_config(tls_config(&host_roots()))
        .build()
        .into();
    let mut response = agent
        .get(&url)
        .header("Authorization", format!("Bearer {}", oidc.key))
        .call()
        .map_err(|e| failed(e.to_string()))?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_TOKEN_ANSWER)
        .read_to_vec()
        .map_err(|e| failed(e.to_string()))?;
    let answer: Value =
        serde_json::from_slice(&body).map_err(|_| failed("the answer is not JSON".to_owned()))?;
    answer
        .get("value")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| failed("the answer has no `value`".to_owned()))
}

/// The three credentials of an STS answer (`--query Credentials`).
fn credentials(stdout: &str) -> Result<[String; 3], Error> {
    let unusable = || Error::Failed("the credentials answer of STS is unusable".to_owned());
    let answer: Value = serde_json::from_str(stdout).map_err(|_| unusable())?;
    let field = |name: &str| {
        answer
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty() && !v.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or_else(unusable)
    };
    Ok([
        field("AccessKeyId")?,
        field("SecretAccessKey")?,
        field("SessionToken")?,
    ])
}

/// Writes `::add-mask::<value>` to the log, now.
fn mask(log: &mut dyn Write, value: &str) -> Result<(), Error> {
    log.write_all(format!("::add-mask::{value}\n").as_bytes())
        .and_then(|()| log.flush())
        .map_err(Error::spawn("write the log".to_owned()))
}

/// The `assume` verb: the lines to print after the masks.
///
/// # Errors
/// An empty role, a session name or region that is not plain, a token request that fails, a
/// refusal by STS, or an env file that cannot be appended to. No credential is in the message.
pub fn assume(
    aws: &dyn Aws,
    log: &mut dyn Write,
    oidc: &dyn Fn() -> Result<TokenService, Error>,
    request: &Request,
) -> Result<Vec<String>, Error> {
    if request.role_arn.is_empty() {
        return Err(Error::Usage("--role-arn is empty".to_owned()));
    }
    plain("--role-arn", request.role_arn)?;
    plain("--region", request.region)?;
    if !session_ok(request.session_name) {
        return Err(Error::Usage(
            "--session-name is not 2 to 64 letters, digits and `_+=,.@-`".to_owned(),
        ));
    }
    let oidc = oidc()?;
    // The role carries the account id: kept out of the log, as the action's
    // `mask-aws-account-id` did.
    mask(log, request.role_arn)?;
    mask(log, &oidc.key)?;
    let jwt = request_token(&oidc, request.https_only)?;
    mask(log, &jwt)?;
    let token = TokenFile::write(request.token_dir, &jwt)?;
    let token_argument = token.argument();
    let answer = aws.run(
        &[
            "sts",
            "assume-role-with-web-identity",
            "--role-arn",
            request.role_arn,
            "--role-session-name",
            request.session_name,
            "--web-identity-token",
            &token_argument,
            "--duration-seconds",
            DURATION_SECONDS,
            "--query",
            "Credentials",
            "--output",
            "json",
        ]
        .map(str::to_owned),
    );
    drop(token);
    let answer = answer?;
    if !answer.ok {
        // The refusal of STS names the role and the reason, never a credential; the token it
        // may quote is masked already.
        return Err(Error::Failed(format!(
            "assuming the role failed: {}",
            answer.stderr.trim()
        )));
    }
    let [key, secret, session] = credentials(&answer.stdout)?;
    for value in [&key, &secret, &session] {
        mask(log, value)?;
    }
    super::append_output(
        request.env_file,
        &[
            format!("AWS_ACCESS_KEY_ID={key}"),
            format!("AWS_SECRET_ACCESS_KEY={secret}"),
            format!("AWS_SESSION_TOKEN={session}"),
            format!("AWS_REGION={}", request.region),
            format!("AWS_DEFAULT_REGION={}", request.region),
        ],
    )?;
    Ok(vec![
        "assumed the role; its credentials are masked and reach the steps after this one"
            .to_owned(),
    ])
}

#[cfg(test)]
mod tests;
