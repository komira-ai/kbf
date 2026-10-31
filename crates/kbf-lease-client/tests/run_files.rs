//! Run files through the client and the fake: what a start with files carries, what the
//! fake refuses and with which code, and that no answer, listing or text repeats a content.

mod support;

use kbf_lease_client::fake::{FakeLeaseServer, Pool};
use kbf_lease_client::{Client, ErrorCode, LeaseId, RunFile, RunSpec};
use support::{fake, lease, pool, raw_json, refused, run_of};

const SECRET: &str = "https://store.example/put?X-Amz-Signature=SECRET-VALUE-123";

fn file(name: &str, contents: &str) -> RunFile {
    RunFile {
        name: name.to_owned(),
        contents: contents.to_owned(),
    }
}

fn with_files(files: Vec<RunFile>) -> RunSpec {
    let mut spec = run_of(100, 1);
    spec.files = files;
    spec
}

/// A fake whose hosts report `run-files`, a client of it and a lease on one host.
async fn files_fake() -> (FakeLeaseServer, Client, LeaseId) {
    let server = FakeLeaseServer::start(pool().with_run_files()).await;
    let client = server.client().unwrap();
    let id = lease(&client).await;
    (server, client, id)
}

fn name_of(len: usize) -> String {
    "n".repeat(len)
}

/// Asserts that `Debug` of a file prints its name and the length of its contents, that the
/// spec's `Debug` prints only a count, and that neither prints a content. It catches a
/// derived `Debug`, which is what a log line of a failed call would print.
#[test]
fn debug_of_a_spec_and_a_file_never_shows_contents() {
    let one = file("put-url", SECRET);
    let printed = format!("{one:?}");
    assert!(!printed.contains("SECRET-VALUE"), "{printed}");
    assert!(printed.contains("put-url"), "{printed}");
    assert!(
        printed.contains(&format!("contents_len: {}", SECRET.len())),
        "{printed}"
    );
    let spec = with_files(vec![one, file("grant-token", "tok-SECRET-VALUE-456")]);
    let printed = format!("{spec:?}");
    assert!(!printed.contains("SECRET-VALUE"), "{printed}");
    assert!(printed.contains("files: <2 redacted>"), "{printed}");
    let alternate = format!("{spec:#?}");
    assert!(!alternate.contains("SECRET-VALUE"), "{alternate}");
}

/// Asserts that a start with files for a host that did not report `run-files` is `409
/// files-unsupported` and queues nothing, and that the same start is accepted on a pool whose
/// hosts report it. It catches a fake that accepts files anywhere (an agent without the
/// capability would run the job without its secrets) and one that refuses them everywhere.
#[tokio::test]
async fn the_fake_refuses_files_for_a_host_without_run_files() {
    let (_server, client) = fake().await;
    let id = lease(&client).await;
    let error = client
        .queue_run(id, &with_files(vec![file("put-url", SECRET)]))
        .await
        .unwrap_err();
    assert_eq!(refused(error), (409, ErrorCode::FilesUnsupported));
    assert!(client.runs(id).await.unwrap().is_empty());

    let (server_with, client_with, id_with) = files_fake().await;
    let queued = client_with
        .queue_run(id_with, &with_files(vec![file("put-url", SECRET)]))
        .await
        .unwrap();
    let kept = server_with.run_files(&queued.run_id).unwrap();
    assert_eq!(kept, [file("put-url", SECRET)]);
    // A start without files needs no capability.
    assert!(client.queue_run(id, &run_of(100, 1)).await.is_ok());
}

/// Asserts that a pool made with `with_run_files` says so in its host list, and that a
/// default pool does not. It catches a fake whose answer and whose behaviour disagree.
#[tokio::test]
async fn the_fake_reports_run_files_as_a_capability_only_when_asked() {
    let (_server, client) = fake().await;
    let hosts = client.hosts().await.unwrap();
    assert!(!hosts[0].capabilities.contains_key("run-files"));
    let server = FakeLeaseServer::start(Pool::with_run_files(pool())).await;
    let hosts = server.client().unwrap().hosts().await.unwrap();
    assert_eq!(hosts[0].capabilities["run-files"], "true");
    assert_eq!(hosts[0].capabilities["os"], "linux");
}

/// Asserts each rule of the file limits at its edge: the value at the limit is accepted and
/// one more is a `400 invalid-file` whose body is exactly the code, with no reason, and whose
/// error text repeats neither a name nor a content. The rules are the lease server's: at most
/// 8 files; a name of `[A-Za-z0-9._-]`, leading alphanumeric, at most 64 bytes, unique ignoring
/// case; 16 KiB a file; 64 KiB in all; no NUL. It catches a rule missing from the fake and an
/// error that echoes a secret.
#[tokio::test]
async fn the_fake_refuses_each_bad_file_without_echoing_it() {
    let (server, client, id) = files_fake().await;
    let big = "x".repeat(16 * 1024);
    let many = |n: usize| -> Vec<RunFile> { (0..n).map(|i| file(&format!("f{i}"), "x")).collect() };
    let four_big: Vec<RunFile> = (0..4).map(|i| file(&format!("b{i}"), &big)).collect();
    let mut five_big = four_big.clone();
    five_big.push(file("b4", "x"));
    let accepted: [(&str, Vec<RunFile>); 9] = [
        ("eight files", many(8)),
        ("a 64 byte name", vec![file(&name_of(64), SECRET)]),
        ("dots dashes and underscores", vec![file("a.b-c_d", SECRET)]),
        ("a leading digit", vec![file("1a", SECRET)]),
        ("16 KiB", vec![file("a", &big)]),
        ("64 KiB in all", four_big),
        ("no files", Vec::new()),
        ("names apart", vec![file("a", "x"), file("b", "x")]),
        ("a newline in contents", vec![file("a", "line\nline")]),
    ];
    for (what, files) in accepted {
        let spec = with_files(files);
        assert!(client.queue_run(id, &spec).await.is_ok(), "{what}");
    }
    let refused_cases: [(&str, Vec<RunFile>); 11] = [
        ("nine files", many(9)),
        ("a 65 byte name", vec![file(&name_of(65), SECRET)]),
        ("an empty name", vec![file("", SECRET)]),
        ("a leading dot", vec![file(".secret-name", SECRET)]),
        ("a leading dash", vec![file("-secret-name", SECRET)]),
        ("a slash in the name", vec![file("../secret-name", SECRET)]),
        ("a space in the name", vec![file("secret name", SECRET)]),
        (
            "a name twice ignoring case",
            vec![file("Secret-Name", SECRET), file("secret-name", "y")],
        ),
        (
            "16 KiB and one byte",
            vec![file("secret-name", &format!("{big}x"))],
        ),
        ("64 KiB and one byte in all", five_big),
        ("a NUL", vec![file("secret-name", &format!("{SECRET}\0"))]),
    ];
    for (what, files) in refused_cases {
        let spec = with_files(files);
        let error = client.queue_run(id, &spec).await.unwrap_err();
        for text in [format!("{error}"), format!("{error:?}")] {
            assert!(!text.contains("SECRET-VALUE"), "{what}: {text}");
            assert!(!text.contains("secret-name"), "{what}: {text}");
        }
        assert_eq!(refused(error), (400, ErrorCode::InvalidFile), "{what}");
        let body = serde_json::to_string(&spec).unwrap();
        let path = format!("/v1/leases/{id}/runs");
        let (status, answer) = raw_json(&server, "POST", &path, &body).await;
        assert_eq!(
            (status, answer),
            (400, serde_json::json!({ "error": "invalid-file" })),
            "{what}"
        );
    }
}

/// Asserts that a bad file is answered before the lease is looked at, as the lease server
/// does: a start with a bad file for a lease nobody holds is `400 invalid-file`, not `404`.
/// It catches a fake that checks the lease first, which would hide a bad file from a test
/// that has not acquired yet.
#[tokio::test]
async fn a_bad_file_is_answered_before_the_lease_is_looked_at() {
    let (_server, client, _) = files_fake().await;
    let nobody: LeaseId = "4-99".parse().unwrap();
    let bad = with_files(vec![file("../x", "y")]);
    let error = client.queue_run(nobody, &bad).await.unwrap_err();
    assert_eq!(refused(error), (400, ErrorCode::InvalidFile));
    let good = with_files(vec![file("x", "y")]);
    let error = client.queue_run(nobody, &good).await.unwrap_err();
    assert_eq!(refused(error), (404, ErrorCode::UnknownLease));
}

/// Asserts that no body the fake serves holds a content: the 202 of the start, the run, the
/// run list, the lease list and the host list, read raw, while the fake's own accessor still
/// has the files as they were sent. It catches a fake that echoes the spec back, which the
/// lease server never does.
#[tokio::test]
async fn the_fake_never_lists_contents() {
    let (server, client, id) = files_fake().await;
    let spec = with_files(vec![
        file("put-url", SECRET),
        file("grant-token", "tok-SECRET-VALUE-9"),
    ]);
    let body = serde_json::to_string(&spec).unwrap();
    let (status, accepted) =
        raw_json(&server, "POST", &format!("/v1/leases/{id}/runs"), &body).await;
    assert_eq!(status, 202, "{accepted}");
    let run = accepted["run_id"].as_str().unwrap().to_owned();
    let mut bodies = vec![accepted.to_string()];
    for path in [
        format!("/v1/leases/{id}/runs/{run}"),
        format!("/v1/leases/{id}/runs"),
        "/v1/leases".to_owned(),
        "/v1/hosts".to_owned(),
    ] {
        let (status, body) = raw_json(&server, "GET", &path, "").await;
        assert_eq!(status, 200, "{path}");
        bodies.push(body.to_string());
    }
    for body in &bodies {
        assert!(!body.contains("SECRET-VALUE"), "{body}");
        assert!(!body.contains("put-url"), "{body}");
    }
    let shown = client.runs(id).await.unwrap();
    assert!(!format!("{shown:?}").contains("SECRET-VALUE"));
    let run_id = shown[0].run_id.clone();
    assert_eq!(server.run_files(&run_id), Some(spec.files));
    assert_eq!(server.run_files(&"run-4-99".parse().unwrap()), None);
}

/// Asserts the order the lease server refuses a start in: a host that cannot deliver files is
/// `409 files-unsupported` whether it is lost, tainted or refusing runs, and the same host is
/// still refused for its own reason when the start has no files. It catches a fake that tests
/// the host's state first: a caller that retries elsewhere on a host fault and gives up on
/// "this job would run without its secrets" would see a different answer from the real server.
#[tokio::test]
async fn a_lost_or_tainted_host_without_run_files_is_refused_files_unsupported() {
    type Break = fn(&FakeLeaseServer);
    let cases: [(&str, Break, ErrorCode); 3] = [
        (
            "lost",
            |server| assert!(server.set_lost("node1", true)),
            ErrorCode::HostLost,
        ),
        (
            "tainted",
            |server| assert!(server.taint_host("node1", "scratch-not-clean")),
            ErrorCode::HostTainted,
        ),
        (
            "refusing",
            |server| assert!(server.refuse_posts("node1", Some("quota-unavailable"))),
            ErrorCode::QuotaUnavailable,
        ),
    ];
    for (what, break_host, own) in cases {
        let (server, client) = fake().await;
        let id = lease(&client).await;
        break_host(&server);
        let with = client
            .queue_run(id, &with_files(vec![file("put-url", SECRET)]))
            .await
            .unwrap_err();
        assert_eq!(refused(with), (409, ErrorCode::FilesUnsupported), "{what}");
        let without = client.queue_run(id, &run_of(100, 1)).await.unwrap_err();
        assert_eq!(refused(without), (409, own), "{what}: no files");
    }
}

/// Asserts that a file name with a non-ASCII character is refused: the grammar is checked
/// byte by byte, so an accented letter, a multibyte letter and an emoji are not alphanumeric
/// or punctuation of the grammar, in any position. It catches a check written on `char`s that
/// takes `is_alphanumeric` for Unicode letters.
#[tokio::test]
async fn a_non_ascii_file_name_is_refused() {
    let (_server, client, id) = files_fake().await;
    for name in ["é", "aé", "日本", "a\u{1F600}", "\u{FF21}bc"] {
        let error = client
            .queue_run(id, &with_files(vec![file(name, "x")]))
            .await
            .unwrap_err();
        assert_eq!(refused(error), (400, ErrorCode::InvalidFile), "{name:?}");
    }
}
