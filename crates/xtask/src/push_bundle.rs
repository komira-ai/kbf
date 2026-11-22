//! `push-task-bundle --asset <tarball> --repository <host>/<path> --tag <tag> --summary <file>`: the
//! released macOS `kbf-task` bundle in an OCI registry, by digest.
//!
//! The asset is the tarball `package-task-bundle` wrote ([`crate::task_bundle`]): an OCI image layout.
//! It is read into its files and refused unless it is exactly one `kbf-task` bundle
//! ([`task_bundle::check_layout`]). Then, with the OCI distribution protocol over HTTPS to
//! `https://<host>`:
//!
//! 1. `GET /v2/`: 200 needs no credential; 401 with a `Bearer` challenge is answered by one token
//!    request to the challenge's realm (HTTP Basic, user `x-access-token`, password `GH_TOKEN`), and
//!    the token is sent as `Authorization: Bearer` afterwards. A realm that is not HTTPS is refused.
//! 2. The config and the layer, each: `HEAD` its blob; present means skipped, else `POST` an upload
//!    and `PUT` the bytes with their digest.
//! 3. The manifest, as the layout holds it, `PUT` under the tag, then under its digest, and a `HEAD`
//!    of the digest. A `Docker-Content-Digest` that is not the layout's digest is refused.
//!
//! A re-run sends the same bytes and succeeds. The line to print is `<repository>@sha256:<hex>`, the
//! value of the frontend's `--executor-mac-task-bundle`; it is also appended to `--summary`. The
//! credential is read from `GH_TOKEN` and is in no error and no output.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use clap::Args as ClapArgs;

use crate::fetch::{self, Sha256};
use crate::task_bundle::{self, Files};
use crate::{Error, package};

/// The media type of a manifest the layout does not name one for.
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// The most one request may take, a layer upload included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The user name the registry's token endpoint is given beside the token.
const TOKEN_USER: &str = "x-access-token";

/// The flags of the verb.
#[derive(ClapArgs)]
pub struct Args {
    /// The release asset `kbf-task-bundle-<commit>-darwin-arm64.tar.gz`.
    #[arg(long)]
    pub asset: PathBuf,
    /// The repository to push to, `<host>/<path>`, lower case, without a scheme.
    #[arg(long)]
    pub repository: String,
    /// The tag the manifest is also put under.
    #[arg(long)]
    pub tag: String,
    /// A file to append the pull reference to, as markdown; created when missing.
    #[arg(long)]
    pub summary: PathBuf,
}

/// Where to push: the registry's base URL, the repository path in it and the tag.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// `https://<host>`, without a trailing `/`.
    pub base: String,
    /// The repository as given, `<host>/<path>`.
    pub repository: String,
    /// The path below the host.
    pub path: String,
    /// The tag.
    pub tag: String,
}

impl Target {
    /// The target of `repository` (`<host>/<path>`) and `tag`, over HTTPS.
    pub fn parse(repository: &str, tag: &str) -> Result<Target, Error> {
        let bad = |why: &str| Error::Failed(format!("--repository {repository:?}: {why}"));
        let Some((host, path)) = repository.split_once('/') else {
            return Err(bad("expected <host>/<path>"));
        };
        let host_ok = !host.is_empty()
            && host
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b':'));
        let path_ok = path.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
        });
        if !host_ok || !path_ok {
            return Err(bad(
                "expected lower-case <host>/<path>, with no scheme and no empty part",
            ));
        }
        let mut chars = tag.chars();
        let tag_ok = tag.len() <= 128
            && chars
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !tag_ok {
            return Err(Error::Failed(format!("--tag {tag:?} is not an OCI tag")));
        }
        Ok(Target {
            base: format!("https://{host}"),
            repository: repository.to_owned(),
            path: path.to_owned(),
            tag: tag.to_owned(),
        })
    }
}

/// The credential: `value` (the environment's `GH_TOKEN`), refused when unset or empty.
pub fn token_from(value: Option<String>) -> Result<String, Error> {
    match value {
        Some(token) if !token.is_empty() => Ok(token),
        _ => Err(Error::Failed(
            "GH_TOKEN is not set (or is empty): the registry credential".to_owned(),
        )),
    }
}

/// The files of the layout tarball `tgz`: regular files at their paths; directories are
/// not files of a layout and are passed over; anything else is refused.
pub fn read_tarball(tgz: &[u8]) -> Result<Files, Error> {
    let bad = |why: String| Error::Failed(format!("the asset is not a layout tarball: {why}"));
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(tgz)
        .read_to_end(&mut tar)
        .map_err(|e| bad(format!("not a gzip stream: {e}")))?;
    let mut files = Files::new();
    let mut archive = tar::Archive::new(&tar[..]);
    let entries = archive.entries().map_err(|e| bad(e.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        match entry.header().entry_type() {
            tar::EntryType::Directory => {}
            tar::EntryType::Regular => {
                let mut bytes = Vec::new();
                entry
                    .read_to_end(&mut bytes)
                    .map_err(|e| bad(format!("{path}: {e}")))?;
                if files.insert(path.clone(), bytes).is_some() {
                    return Err(bad(format!("{path} is listed twice")));
                }
            }
            other => {
                return Err(bad(format!(
                    "{path} is {other:?}, not a file or a directory"
                )));
            }
        }
    }
    Ok(files)
}

/// One blob to send: its hex digest and bytes.
struct Blob<'a> {
    hex: String,
    bytes: &'a [u8],
}

/// What a push sends, read from a layout that [`task_bundle::check_layout`] accepted.
struct Plan<'a> {
    manifest: Blob<'a>,
    media_type: String,
    blobs: Vec<Blob<'a>>,
}

/// The blob `descriptor` names, which must be in `files` under its digest with its size.
fn blob<'a>(
    files: &'a Files,
    what: &str,
    descriptor: &serde_json::Value,
) -> Result<Blob<'a>, Error> {
    let bad = |why: &str| Error::Failed(format!("{what}: {why}"));
    let hex = descriptor["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .ok_or_else(|| bad("no sha256 digest"))?;
    let bytes = files
        .get(&format!("blobs/sha256/{hex}"))
        .ok_or_else(|| bad("its blob is not in the layout"))?;
    if Sha256::of(bytes).to_string() != hex
        || descriptor["size"].as_u64() != Some(bytes.len() as u64)
    {
        return Err(bad(
            "its blob is not the digest and size the descriptor says",
        ));
    }
    Ok(Blob {
        hex: hex.to_owned(),
        bytes,
    })
}

fn plan(files: &Files) -> Result<Plan<'_>, Error> {
    let problems = task_bundle::check_layout(files);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let parse = |bytes: &[u8]| -> Result<serde_json::Value, Error> {
        serde_json::from_slice(bytes).map_err(|e| Error::Failed(format!("not JSON: {e}")))
    };
    let index = parse(&files["index.json"])?;
    let entry = &index["manifests"][0];
    let manifest = blob(files, "index.json manifest", entry)?;
    let parsed = parse(manifest.bytes)?;
    let media_type = parsed["mediaType"]
        .as_str()
        .or_else(|| entry["mediaType"].as_str())
        .unwrap_or(OCI_MANIFEST)
        .to_owned();
    let mut blobs = vec![blob(files, "the manifest's config", &parsed["config"])?];
    blobs.push(blob(files, "the manifest's layer", &parsed["layers"][0])?);
    Ok(Plan {
        manifest,
        media_type,
        blobs,
    })
}

/// The client of the registry: plain HTTP is allowed only when `https_only` is false (a test's
/// loopback registry). Redirects are not followed: the credential goes to the registry only.
pub fn agent(https_only: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(https_only)
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(REQUEST_TIMEOUT))
        .tls_config(fetch::tls_config(&fetch::host_roots()))
        .build()
        .into()
}

/// The answer to one request: its status and the headers the protocol reads.
struct Reply {
    status: u16,
    location: Option<String>,
    content_digest: Option<String>,
    challenge: Option<String>,
    body: Vec<u8>,
}

/// A registry session: one agent, one base URL and the `Authorization` header, once there is one.
struct Session<'a> {
    agent: &'a ureq::Agent,
    base: &'a str,
    authorization: Option<String>,
}

impl Session<'_> {
    fn reply(
        &self,
        step: &str,
        url: &str,
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Reply, Error> {
        let mut response = result
            .map_err(|e| Error::Failed(format!("{step}: {}: {e}", path_of(url, self.base))))?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (location, content_digest, challenge) = (
            header("location"),
            header("docker-content-digest"),
            header("www-authenticate"),
        );
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(1 << 20)
            .read_to_vec()
            .unwrap_or_default();
        Ok(Reply {
            status,
            location,
            content_digest,
            challenge,
            body,
        })
    }

    fn get(&self, step: &str, url: &str, authorization: Option<&str>) -> Result<Reply, Error> {
        let mut request = self.agent.get(url);
        if let Some(value) = authorization.or(self.authorization.as_deref()) {
            request = request.header("Authorization", value);
        }
        self.reply(step, url, request.call())
    }

    fn head(&self, step: &str, url: &str) -> Result<Reply, Error> {
        let mut request = self.agent.head(url);
        if let Some(value) = &self.authorization {
            request = request.header("Authorization", value);
        }
        self.reply(step, url, request.call())
    }

    fn post(&self, step: &str, url: &str) -> Result<Reply, Error> {
        let mut request = self.agent.post(url);
        if let Some(value) = &self.authorization {
            request = request.header("Authorization", value);
        }
        self.reply(step, url, request.send(&[][..]))
    }

    fn put(&self, step: &str, url: &str, content_type: &str, bytes: &[u8]) -> Result<Reply, Error> {
        let mut request = self.agent.put(url).header("Content-Type", content_type);
        if let Some(value) = &self.authorization {
            request = request.header("Authorization", value);
        }
        self.reply(step, url, request.send(bytes))
    }
}

/// `url` as the path (and query) below `base`, for an error; a URL elsewhere as given.
fn path_of<'a>(url: &'a str, base: &str) -> &'a str {
    url.strip_prefix(base).unwrap_or(url)
}

/// An error for `step` whose reply had an unexpected status.
fn unexpected(step: &str, url: &str, base: &str, status: u16) -> Error {
    Error::Failed(format!(
        "{step}: {} answered status {status}",
        path_of(url, base)
    ))
}

/// The value of `key` in the parameters of a `Bearer` challenge.
fn challenge_param(challenge: &str, key: &str) -> Option<String> {
    let rest = challenge.strip_prefix("Bearer ")?;
    rest.split(',').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == key).then(|| v.trim_matches('"').to_owned())
    })
}

/// A token realm is HTTPS, unless the registry itself is plain HTTP (a test's loopback registry).
fn check_realm(realm: &str, base: &str) -> Result<(), Error> {
    if realm.starts_with("https://") || base.starts_with("http://") {
        return Ok(());
    }
    Err(Error::Failed(format!(
        "check the registry: the token realm {realm:?} is not https"
    )))
}

/// The `Authorization` header the registry wants for pushing to `target`: none when `/v2/` needs no
/// credential, else `Bearer` with the token the challenge's realm gives for `token`.
fn authorize(session: &Session<'_>, target: &Target, token: &str) -> Result<Option<String>, Error> {
    let url = format!("{}/v2/", target.base);
    let ping = session.get("check the registry", &url, None)?;
    if ping.status == 200 {
        return Ok(None);
    }
    if ping.status != 401 {
        return Err(unexpected(
            "check the registry",
            &url,
            &target.base,
            ping.status,
        ));
    }
    let challenge = ping.challenge.unwrap_or_default();
    let Some(realm) = challenge_param(&challenge, "realm") else {
        return Err(Error::Failed(
            "check the registry: /v2/ answered 401 without a Bearer challenge that names a realm"
                .to_owned(),
        ));
    };
    check_realm(&realm, &target.base)?;
    let mut query = format!("scope=repository:{}:pull,push", target.path);
    if let Some(service) = challenge_param(&challenge, "service") {
        query = format!("service={service}&{query}");
    }
    let join = if realm.contains('?') { '&' } else { '?' };
    let token_url = format!("{realm}{join}{query}");
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{TOKEN_USER}:{token}"));
    let reply = session.get("get a token", &token_url, Some(&format!("Basic {basic}")))?;
    if reply.status != 200 {
        return Err(Error::Failed(format!(
            "get a token: {} answered status {}",
            realm.split('?').next().unwrap_or_default(),
            reply.status
        )));
    }
    let json: serde_json::Value = serde_json::from_slice(&reply.body).unwrap_or_default();
    let bearer = json["token"]
        .as_str()
        .or_else(|| json["access_token"].as_str());
    match bearer {
        Some(bearer) => Ok(Some(format!("Bearer {bearer}"))),
        None => Err(Error::Failed(
            "get a token: the answer has no token".to_owned(),
        )),
    }
}

/// `location` of an upload, as an absolute URL: relative to `base`, or on it.
fn upload_url(location: Option<&str>, base: &str) -> Result<String, Error> {
    match location {
        Some(l) if l.starts_with('/') => Ok(format!("{base}{l}")),
        Some(l) if l.starts_with(&format!("{base}/")) => Ok(l.to_owned()),
        _ => Err(Error::Failed(
            "start an upload: the Location is missing or not on the registry".to_owned(),
        )),
    }
}

/// Sends `blob` unless the registry has it.
fn send_blob(session: &Session<'_>, target: &Target, blob: &Blob<'_>) -> Result<(), Error> {
    let base = &target.base;
    let repo = format!("{base}/v2/{}", target.path);
    let url = format!("{repo}/blobs/sha256:{}", blob.hex);
    if session.head("look for a blob", &url)?.status == 200 {
        return Ok(());
    }
    let start = format!("{repo}/blobs/uploads/");
    let reply = session.post("start an upload", &start)?;
    if reply.status != 202 {
        return Err(unexpected("start an upload", &start, base, reply.status));
    }
    let location = upload_url(reply.location.as_deref(), base)?;
    let join = if location.contains('?') { '&' } else { '?' };
    let put = format!("{location}{join}digest=sha256:{}", blob.hex);
    let reply = session.put(
        "upload a blob",
        &put,
        "application/octet-stream",
        blob.bytes,
    )?;
    if reply.status != 201 {
        return Err(unexpected("upload a blob", &put, base, reply.status));
    }
    Ok(())
}

/// Pushes the bundle `files` to `target` with `agent`, authorizing with `token`: the pull
/// reference `<repository>@sha256:<hex>`.
pub fn push(
    files: &Files,
    target: &Target,
    token: &str,
    agent: &ureq::Agent,
) -> Result<String, Error> {
    let plan = plan(files)?;
    let mut session = Session {
        agent,
        base: &target.base,
        authorization: None,
    };
    session.authorization = authorize(&session, target, token)?;
    for blob in &plan.blobs {
        send_blob(&session, target, blob)?;
    }
    let digest = format!("sha256:{}", plan.manifest.hex);
    let repo = format!("{}/v2/{}", target.base, target.path);
    for reference in [target.tag.as_str(), digest.as_str()] {
        let url = format!("{repo}/manifests/{reference}");
        let reply = session.put(
            "put the manifest",
            &url,
            &plan.media_type,
            plan.manifest.bytes,
        )?;
        if reply.status != 201 {
            return Err(unexpected(
                "put the manifest",
                &url,
                &target.base,
                reply.status,
            ));
        }
        if let Some(said) = reply.content_digest.filter(|d| *d != digest) {
            return Err(Error::Failed(format!(
                "put the manifest: {} answered Docker-Content-Digest {said}, not {digest}",
                path_of(&url, &target.base)
            )));
        }
    }
    let url = format!("{repo}/manifests/{digest}");
    let reply = session.head("check the manifest", &url)?;
    if reply.status != 200 {
        return Err(unexpected(
            "check the manifest",
            &url,
            &target.base,
            reply.status,
        ));
    }
    Ok(format!("{}@{digest}", target.repository))
}

/// Appends the section naming `reference` to `summary`.
fn append_summary(summary: &Path, reference: &str) -> Result<(), Error> {
    let what = format!("append to {}", summary.display());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(summary)
        .map_err(Error::spawn(what.clone()))?;
    writeln!(
        file,
        "\n### macOS kbf-task bundle\n\nThe macOS kbf-task bundle is pushed: `{reference}`, the value of the frontend's `--executor-mac-task-bundle`."
    )
    .map_err(Error::spawn(what))
}

/// The verb: the asset pushed with the host's roots, the reference printed and appended to the summary.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let target = Target::parse(&args.repository, &args.tag)?;
    run_with(args, &target, std::env::var("GH_TOKEN").ok(), &agent(true))
}

/// [`run`] for `target`, the credential `token` (the environment's `GH_TOKEN`) and `agent`.
pub fn run_with(
    args: &Args,
    target: &Target,
    token: Option<String>,
    agent: &ureq::Agent,
) -> Result<Vec<String>, Error> {
    let token = token_from(token)?;
    let files = read_tarball(&package::read(&args.asset)?)?;
    let reference = push(&files, target, &token, agent)?;
    append_summary(&args.summary, &reference)?;
    Ok(vec![reference])
}

#[cfg(test)]
mod tests;
