use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use base64::Engine;
use kbf_testkit::dir::TestDir;

use super::*;

const SECRET: &str = "ghs_secret_token_value";
const ISSUED: &str = "issued-bearer";
const REPOSITORY: &str = "ghcr.io/komira-ai/kbf-task";
const PATH: &str = "komira-ai/kbf-task";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-push-bundle-unit"), name)
}

fn digest(bytes: &[u8]) -> String {
    Sha256::of(bytes).to_string()
}

fn descriptor(bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({"digest": format!("sha256:{}", digest(bytes)), "size": bytes.len()})
}

/// A valid one-layer `kbf-task` layout; `layer` is the layer's bytes.
fn layout(layer: &[u8], media_type: Option<&str>) -> Files {
    let config = serde_json::to_vec(&serde_json::json!({
        "os": "darwin",
        "architecture": "arm64",
        "name": "kbf-task",
        "layers": [{"tree": "task"}],
        "entrypoints": {"kbf-task": {"path": "${tree:task}/kbf-task"}},
    }))
    .expect("config");
    let mut manifest = serde_json::json!({
        "config": descriptor(&config),
        "layers": [descriptor(layer)],
    });
    if let Some(m) = media_type {
        manifest["mediaType"] = m.into();
    }
    let manifest = serde_json::to_vec(&manifest).expect("manifest");
    let index = serde_json::to_vec(&serde_json::json!({"manifests": [descriptor(&manifest)]}))
        .expect("index");
    let mut files = Files::new();
    files.insert(
        "oci-layout".to_owned(),
        br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
    );
    files.insert("index.json".to_owned(), index);
    for blob in [layer.to_vec(), config, manifest] {
        files.insert(format!("blobs/sha256/{}", digest(&blob)), blob);
    }
    files
}

fn good() -> Files {
    layout(b"the layer's bytes", None)
}

/// The hex digest of the manifest of `files`.
fn manifest_hex(files: &Files) -> String {
    let index: serde_json::Value = serde_json::from_slice(&files["index.json"]).expect("index");
    index["manifests"][0]["digest"]
        .as_str()
        .expect("digest")
        .trim_start_matches("sha256:")
        .to_owned()
}

/// What the fake registry does other than the protocol's happy path.
#[derive(Default, Clone)]
struct Quirks {
    /// `/v2/` needs no credential.
    open: bool,
    /// `/v2/` answers this status instead of 401.
    ping_status: Option<u16>,
    /// The `WWW-Authenticate` value instead of the Bearer challenge of the fake's own realm
    /// (`{base}` is the fake's URL).
    challenge: Option<String>,
    /// The token endpoint answers this status.
    token_status: Option<u16>,
    /// The JSON key the token is under; `none` for no token at all.
    token_key: Option<&'static str>,
    /// The upload `POST` answers this status.
    post_status: Option<u16>,
    /// The `Location` of an upload: `none` for no header (`{base}` is the fake's URL).
    location: Option<String>,
    /// The blob `PUT` answers this status.
    put_blob_status: Option<u16>,
    /// A manifest `PUT` answers this status.
    put_manifest_status: Option<u16>,
    /// The `Docker-Content-Digest` of a manifest `PUT`.
    content_digest: Option<String>,
    /// The manifest `HEAD` answers this status.
    head_manifest_status: Option<u16>,
}

#[derive(Default)]
struct State {
    blobs: BTreeMap<String, Vec<u8>>,
    manifests: BTreeMap<String, (String, Vec<u8>)>,
    log: Vec<String>,
    basic_ok: bool,
}

struct Fake {
    base: String,
    state: Arc<Mutex<State>>,
}

struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_owned()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let mut out = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.write_all(body);
}

fn token_endpoint(
    state: &Mutex<State>,
    quirks: &Quirks,
    req: &Request,
    query: &str,
    out: &mut TcpStream,
) {
    let auth = req
        .headers
        .get("authorization")
        .cloned()
        .unwrap_or_default();
    let basic =
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{SECRET}"));
    let ok = auth == format!("Basic {basic}")
        && query.contains("scope=repository:komira-ai/kbf-task:pull,push")
        && query.contains("service=fake");
    state.lock().expect("state").basic_ok = ok;
    if let Some(status) = quirks.token_status {
        return respond(out, status, &[], b"");
    }
    let body = match quirks.token_key {
        Some("none") => "{}".to_owned(),
        Some(key) => format!("{{\"{key}\":\"{ISSUED}\"}}"),
        None => format!("{{\"token\":\"{ISSUED}\"}}"),
    };
    respond(out, if ok { 200 } else { 403 }, &[], body.as_bytes());
}

fn serve(state: &Mutex<State>, quirks: &Quirks, base: &str, req: &Request, out: &mut TcpStream) {
    let (path, query) = req
        .target
        .split_once('?')
        .unwrap_or((req.target.as_str(), ""));
    state
        .lock()
        .expect("state")
        .log
        .push(format!("{} {}", req.method, req.target));
    let auth = req
        .headers
        .get("authorization")
        .cloned()
        .unwrap_or_default();
    let repo = format!("/v2/{PATH}");
    if path == "/token" {
        return token_endpoint(state, quirks, req, query, out);
    }
    let authorized = quirks.open || auth == format!("Bearer {ISSUED}");
    if path == "/v2/" {
        if let Some(status) = quirks.ping_status {
            return respond(out, status, &[], b"");
        }
        if authorized {
            return respond(out, 200, &[], b"{}");
        }
        let challenge = quirks
            .challenge
            .as_deref()
            .map(|c| c.replace("{base}", base))
            .unwrap_or_else(|| {
                format!("Bearer realm=\"{base}/token\",service=\"fake\",scope=\"repository:{PATH}:pull,push\"")
            });
        return respond(out, 401, &[("WWW-Authenticate", challenge)], b"");
    }
    if !authorized {
        return respond(out, 401, &[], b"");
    }
    let mut st = state.lock().expect("state");
    if let Some(hex) = path.strip_prefix(&format!("{repo}/blobs/sha256:")) {
        let status = if st.blobs.contains_key(hex) { 200 } else { 404 };
        return respond(out, status, &[], b"");
    }
    if path == format!("{repo}/blobs/uploads/") {
        let status = quirks.post_status.unwrap_or(202);
        let location = match quirks.location.as_deref() {
            Some("none") => None,
            Some(l) => Some(l.replace("{base}", base)),
            None => Some(format!("{repo}/blobs/uploads/u1")),
        };
        let headers: Vec<(&str, String)> = location.map(|l| ("Location", l)).into_iter().collect();
        return respond(out, status, &headers, b"");
    }
    if path.starts_with(&format!("{repo}/blobs/uploads/")) {
        let want = query
            .split('&')
            .find_map(|q| q.strip_prefix("digest=sha256:"))
            .unwrap_or_default();
        if let Some(status) = quirks.put_blob_status {
            return respond(out, status, &[], b"");
        }
        let typed = req.headers.get("content-type").map(String::as_str);
        if digest(&req.body) != want || typed != Some("application/octet-stream") {
            return respond(out, 400, &[], b"");
        }
        st.blobs.insert(want.to_owned(), req.body.clone());
        return respond(out, 201, &[], b"");
    }
    if let Some(reference) = path.strip_prefix(&format!("{repo}/manifests/")) {
        if req.method == "PUT" {
            if let Some(status) = quirks.put_manifest_status {
                return respond(out, status, &[], b"");
            }
            let content_type = req.headers.get("content-type").cloned().unwrap_or_default();
            let said = quirks
                .content_digest
                .clone()
                .unwrap_or_else(|| format!("sha256:{}", digest(&req.body)));
            st.manifests
                .insert(reference.to_owned(), (content_type, req.body.clone()));
            return respond(out, 201, &[("Docker-Content-Digest", said)], b"");
        }
        let present = if st.manifests.contains_key(reference) {
            200
        } else {
            404
        };
        return respond(
            out,
            quirks.head_manifest_status.unwrap_or(present),
            &[],
            b"",
        );
    }
    respond(out, 404, &[], b"");
}

fn fake(quirks: Quirks) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let state = Arc::new(Mutex::new(State::default()));
    let (thread_state, thread_base) = (Arc::clone(&state), base.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if let Some(req) = read_request(&mut stream) {
                serve(&thread_state, &quirks, &thread_base, &req, &mut stream);
            }
        }
    });
    Fake { base, state }
}

impl Fake {
    fn target(&self) -> Target {
        Target {
            base: self.base.clone(),
            repository: REPOSITORY.to_owned(),
            path: PATH.to_owned(),
            tag: "v1".to_owned(),
        }
    }

    fn push(&self, files: &Files) -> Result<String, Error> {
        push(files, &self.target(), SECRET, &agent(false))
    }

    fn log(&self) -> Vec<String> {
        self.state.lock().expect("state").log.clone()
    }

    fn uploads(&self) -> usize {
        self.log()
            .iter()
            .filter(|l| l.starts_with("PUT ") && l.contains("/blobs/uploads/"))
            .count()
    }
}

/// The message of the refusal of `result`, which never holds the credential.
fn refusal(result: Result<String, Error>) -> String {
    let message = result.expect_err("refused").to_string();
    assert!(!message.contains(SECRET), "{message}");
    assert!(!message.contains(ISSUED), "{message}");
    message
}

#[test]
fn push_puts_every_blob_and_the_manifest_by_digest_and_names_the_reference() {
    let files = good();
    let fake = fake(Quirks::default());
    let reference = fake.push(&files).expect("push");
    let hex = manifest_hex(&files);
    assert_eq!(reference, format!("{REPOSITORY}@sha256:{hex}"));
    let state = fake.state.lock().expect("state");
    assert!(
        state.basic_ok,
        "the token was asked for with the credential as Basic"
    );
    assert_eq!(state.blobs.len(), 2, "the config and the layer only");
    for (name, bytes) in &state.blobs {
        assert_eq!(&files[&format!("blobs/sha256/{name}")], bytes, "{name}");
    }
    let manifest = &files[&format!("blobs/sha256/{hex}")];
    let content_type = "application/vnd.oci.image.manifest.v1+json";
    for reference in ["v1".to_owned(), format!("sha256:{hex}")] {
        assert_eq!(
            state.manifests.get(&reference),
            Some(&(content_type.to_owned(), manifest.clone())),
            "{reference}"
        );
    }
}

#[test]
fn push_puts_the_manifest_under_the_media_type_it_names() {
    let media = "application/vnd.docker.distribution.manifest.v2+json";
    let fake = fake(Quirks::default());
    fake.push(&layout(b"layer", Some(media))).expect("push");
    assert_eq!(fake.state.lock().expect("state").manifests["v1"].0, media);
}

#[test]
fn a_second_push_uploads_nothing_and_puts_the_manifest_again() {
    let files = good();
    let fake = fake(Quirks::default());
    let first = fake.push(&files).expect("first");
    assert_eq!(fake.uploads(), 2);
    let second = fake.push(&files).expect("second");
    assert_eq!(first, second);
    assert_eq!(fake.uploads(), 2, "no blob is uploaded again");
    let puts = fake
        .log()
        .iter()
        .filter(|l| l.starts_with("PUT ") && l.contains("/manifests/"))
        .count();
    assert_eq!(puts, 4, "both manifest puts are repeated");
}

#[test]
fn a_registry_that_needs_no_credential_is_not_asked_for_a_token() {
    let fake = fake(Quirks {
        open: true,
        ..Quirks::default()
    });
    fake.push(&good()).expect("push");
    assert!(
        fake.log().iter().all(|l| !l.contains("/token")),
        "{:?}",
        fake.log()
    );
}

#[test]
fn a_token_under_access_token_is_used() {
    let fake = fake(Quirks {
        token_key: Some("access_token"),
        ..Quirks::default()
    });
    fake.push(&good()).expect("push");
}

#[test]
fn a_realm_with_a_query_gets_the_scope_appended_to_it() {
    let fake = fake(Quirks {
        challenge: Some("Bearer realm=\"{base}/token?a=b\",service=\"fake\"".to_owned()),
        ..Quirks::default()
    });
    fake.push(&good()).expect("push");
    let token_request = fake
        .log()
        .into_iter()
        .find(|l| l.contains("/token"))
        .expect("token request");
    assert_eq!(
        token_request,
        "GET /token?a=b&service=fake&scope=repository:komira-ai/kbf-task:pull,push"
    );
}

#[test]
fn a_registry_answering_another_manifest_digest_is_refused() {
    let wrong = format!("sha256:{}", "0".repeat(64));
    let fake = fake(Quirks {
        content_digest: Some(wrong.clone()),
        ..Quirks::default()
    });
    let hex = manifest_hex(&good());
    assert_eq!(
        refusal(fake.push(&good())),
        format!(
            "put the manifest: /v2/{PATH}/manifests/v1 answered Docker-Content-Digest {wrong}, not sha256:{hex}"
        )
    );
}

#[test]
fn a_layout_that_is_not_one_bundle_is_refused_before_any_request() {
    let mut files = good();
    let name = files
        .keys()
        .find(|k| files[*k] == b"the layer's bytes")
        .cloned()
        .expect("layer");
    files.insert(name.clone(), b"other bytes".to_vec());
    let fake = fake(Quirks::default());
    let message = refusal(fake.push(&files));
    assert!(
        message.contains(&format!(
            "{name}: is not stored under the SHA-256 of its bytes"
        )),
        "{message}"
    );
    assert_eq!(fake.log(), Vec::<String>::new());
}

#[test]
fn a_missing_or_empty_token_is_refused_and_a_set_one_is_returned() {
    let wanted = "GH_TOKEN is not set (or is empty): the registry credential";
    assert_eq!(token_from(None).expect_err("unset").to_string(), wanted);
    assert_eq!(
        token_from(Some(String::new()))
            .expect_err("empty")
            .to_string(),
        wanted
    );
    assert_eq!(token_from(Some("t".to_owned())), Ok("t".to_owned()));
}

#[test]
fn a_repository_is_host_and_lower_case_path_with_no_scheme() {
    assert_eq!(
        Target::parse(REPOSITORY, "v1"),
        Ok(Target {
            base: "https://ghcr.io".to_owned(),
            repository: REPOSITORY.to_owned(),
            path: PATH.to_owned(),
            tag: "v1".to_owned(),
        })
    );
    for bad in [
        "ghcr.io",
        "https://ghcr.io/a/b",
        "ghcr.io/Komira/kbf-task",
        "GHCR.io/a/b",
        "/a/b",
        "ghcr.io/",
        "ghcr.io/a//b",
    ] {
        let message = Target::parse(bad, "v1").expect_err(bad).to_string();
        assert!(
            message.starts_with(&format!("--repository {bad:?}: ")),
            "{message}"
        );
    }
    for bad in ["", "-x", "a b", "a/b", &"a".repeat(129)] {
        assert_eq!(
            Target::parse(REPOSITORY, bad).expect_err(bad).to_string(),
            format!("--tag {bad:?} is not an OCI tag")
        );
    }
}

#[test]
fn an_unexpected_status_names_the_step_the_path_and_the_status() {
    let fake = fake(Quirks {
        post_status: Some(500),
        ..Quirks::default()
    });
    assert_eq!(
        refusal(fake.push(&good())),
        format!("start an upload: /v2/{PATH}/blobs/uploads/ answered status 500")
    );
}

#[test]
fn each_failing_step_is_an_error_naming_it() {
    let hex = manifest_hex(&good());
    let locate = "start an upload: the Location is missing or not on the registry";
    let cases: Vec<(Quirks, String)> = vec![
        (
            Quirks {
                ping_status: Some(503),
                ..Quirks::default()
            },
            "check the registry: /v2/ answered status 503".to_owned(),
        ),
        (
            Quirks {
                challenge: Some("Basic realm=\"x\"".to_owned()),
                ..Quirks::default()
            },
            "check the registry: /v2/ answered 401 without a Bearer challenge that names a realm"
                .to_owned(),
        ),
        (
            Quirks {
                token_status: Some(403),
                ..Quirks::default()
            },
            "get a token: http://127.0.0.1:".to_owned(),
        ),
        (
            Quirks {
                token_key: Some("none"),
                ..Quirks::default()
            },
            "get a token: the answer has no token".to_owned(),
        ),
        (
            Quirks {
                location: Some("none".to_owned()),
                ..Quirks::default()
            },
            locate.to_owned(),
        ),
        (
            Quirks {
                location: Some("http://elsewhere.invalid/u".to_owned()),
                ..Quirks::default()
            },
            locate.to_owned(),
        ),
        (
            Quirks {
                put_blob_status: Some(400),
                ..Quirks::default()
            },
            format!("upload a blob: /v2/{PATH}/blobs/uploads/u1?digest=sha256:"),
        ),
        (
            Quirks {
                put_manifest_status: Some(400),
                ..Quirks::default()
            },
            format!("put the manifest: /v2/{PATH}/manifests/v1 answered status 400"),
        ),
        (
            Quirks {
                head_manifest_status: Some(404),
                ..Quirks::default()
            },
            format!("check the manifest: /v2/{PATH}/manifests/sha256:{hex} answered status 404"),
        ),
    ];
    for (quirks, wanted) in cases {
        let message = refusal(fake(quirks).push(&good()));
        assert!(message.starts_with(&wanted), "{message} vs {wanted}");
    }
}

#[test]
fn an_absolute_location_on_the_registry_is_followed() {
    let fake = fake(Quirks {
        location: Some(format!("{{base}}/v2/{PATH}/blobs/uploads/u2?state=x")),
        ..Quirks::default()
    });
    fake.push(&good()).expect("push");
    let wanted = format!("PUT /v2/{PATH}/blobs/uploads/u2?state=x&digest=sha256:");
    assert!(
        fake.log().iter().any(|l| l.starts_with(&wanted)),
        "{:?}",
        fake.log()
    );
}

#[test]
fn a_realm_that_is_not_https_is_refused_for_an_https_registry_only() {
    assert_eq!(
        check_realm("https://auth.example/token", "https://r.example"),
        Ok(())
    );
    assert_eq!(
        check_realm("http://127.0.0.1:1/token", "http://127.0.0.1:1"),
        Ok(())
    );
    assert_eq!(
        check_realm("http://auth.example/token", "https://r.example")
            .expect_err("plain http")
            .to_string(),
        "check the registry: the token realm \"http://auth.example/token\" is not https"
    );
}

#[test]
fn a_connection_that_fails_is_an_error_naming_the_step_and_path() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    drop(listener);
    let target = Target {
        base,
        repository: REPOSITORY.to_owned(),
        path: PATH.to_owned(),
        tag: "v1".to_owned(),
    };
    let message = refusal(push(&good(), &target, SECRET, &agent(false)));
    assert!(
        message.starts_with("check the registry: /v2/: "),
        "{message}"
    );
}

#[test]
fn the_client_for_a_real_registry_refuses_plain_http() {
    let fake = fake(Quirks::default());
    let message = refusal(push(&good(), &fake.target(), SECRET, &agent(true)));
    assert!(
        message.starts_with("check the registry: /v2/: "),
        "{message}"
    );
    assert_eq!(fake.log(), Vec::<String>::new());
}

#[test]
fn a_tarball_reads_back_as_the_layout_it_was_made_from() {
    let files = good();
    let tgz = task_bundle::tarball(&files).expect("tarball");
    assert_eq!(read_tarball(&tgz).expect("read"), files);
}

fn gzip(tar_bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(tar_bytes).expect("gzip");
    gz.finish().expect("finish")
}

#[test]
fn a_tarball_that_is_not_a_layout_of_regular_files_is_refused() {
    let prefix = "the asset is not a layout tarball: ";
    let not_gzip = read_tarball(b"plain").expect_err("not gzip").to_string();
    assert!(
        not_gzip.starts_with(&format!("{prefix}not a gzip stream")),
        "{not_gzip}"
    );

    let garbage = read_tarball(&gzip(&[b'x'; 1024]))
        .expect_err("garbage")
        .to_string();
    assert!(garbage.starts_with(prefix), "{garbage}");

    let mut link = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    link.append_link(&mut h, "index.json", "/etc/passwd")
        .expect("link");
    let message = read_tarball(&gzip(&link.into_inner().expect("tar")))
        .expect_err("link")
        .to_string();
    assert_eq!(
        message,
        format!("{prefix}index.json is Symlink, not a file or a directory")
    );

    let mut twice = tar::Builder::new(Vec::new());
    for _ in 0..2 {
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_cksum();
        twice
            .append_data(&mut h, "oci-layout", &b"x"[..])
            .expect("file");
    }
    let message = read_tarball(&gzip(&twice.into_inner().expect("tar")))
        .expect_err("twice")
        .to_string();
    assert_eq!(message, format!("{prefix}oci-layout is listed twice"));

    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&task_bundle::tarball(&good()).expect("tarball")[..])
        .read_to_end(&mut raw)
        .expect("raw");
    raw.truncate(600);
    let cut = read_tarball(&gzip(&raw)).expect_err("cut").to_string();
    assert!(cut.starts_with(prefix), "{cut}");
}

fn args(dir: &TestDir, files: &Files) -> Args {
    let asset = dir.path().join("asset.tar.gz");
    std::fs::write(&asset, task_bundle::tarball(files).expect("tarball")).expect("asset");
    Args {
        asset,
        repository: REPOSITORY.to_owned(),
        tag: "v1".to_owned(),
        summary: dir.path().join("summary.md"),
    }
}

#[test]
fn the_verb_prints_the_reference_and_appends_it_to_the_summary() {
    let dir = scratch("verb");
    let files = good();
    let args = args(&dir, &files);
    std::fs::write(&args.summary, "# Release\n").expect("summary");
    let fake = fake(Quirks::default());
    let lines = run_with(
        &args,
        &fake.target(),
        Some(SECRET.to_owned()),
        &agent(false),
    )
    .expect("run");
    let reference = format!("{REPOSITORY}@sha256:{}", manifest_hex(&files));
    assert_eq!(lines, vec![reference.clone()]);
    assert_eq!(
        std::fs::read_to_string(&args.summary).expect("summary"),
        format!(
            "# Release\n\n### macOS kbf-task bundle\n\nThe macOS kbf-task bundle is pushed: `{reference}`, the value of the frontend's `--executor-mac-task-bundle`.\n"
        )
    );
}

#[test]
fn the_verb_without_a_token_or_an_asset_sends_nothing() {
    let dir = scratch("verb-refused");
    let args = args(&dir, &good());
    let fake = fake(Quirks::default());
    let message = run_with(&args, &fake.target(), None, &agent(false))
        .expect_err("no token")
        .to_string();
    assert_eq!(
        message,
        "GH_TOKEN is not set (or is empty): the registry credential"
    );
    let gone = Args {
        asset: dir.path().join("missing"),
        ..args
    };
    run_with(
        &gone,
        &fake.target(),
        Some(SECRET.to_owned()),
        &agent(false),
    )
    .expect_err("no asset");
    assert_eq!(fake.log(), Vec::<String>::new());
}

#[test]
fn the_verb_refuses_a_repository_with_no_slash() {
    let dir = scratch("verb-repo");
    let mut args = args(&dir, &good());
    args.repository = "ghcr.io".to_owned();
    assert_eq!(
        run(&args).expect_err("no slash").to_string(),
        "--repository \"ghcr.io\": expected <host>/<path>"
    );
}

#[test]
fn a_summary_that_cannot_be_written_is_an_error() {
    let dir = scratch("summary");
    let message = append_summary(&dir.path().join("no/such/dir/summary.md"), "r")
        .expect_err("no directory")
        .to_string();
    assert!(message.starts_with("append to "), "{message}");
}
