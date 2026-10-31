//! `GET /locate` over the job listener's real TLS: the table is sent as newline-delimited JSON
//! with that content type, and a refusal keeps the text type of the others.

use super::{Timeouts, ca, files, ip, listening, request, world};
use crate::test_dir::scratch_dir;

/// Catches: a table sent as an octet stream (or a refusal sent as JSON): the table of the
/// Action's scope has one JSON line per digest of it (a source with no packs answers each as
/// absent), the stranger's blob is not in it, and a request without a grant is a plain-text 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_table_is_sent_as_json_lines_over_tls() {
    let dir = scratch_dir("job-read-locate");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (addr, _latch) = listening(&files, world.source(), Timeouts::default());
    let got = request(addr, &farm.pem, "GET", "/locate", Some(&world.token()))
        .await
        .expect("connects");
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.header("content-type"), Some("application/x-ndjson"));
    let text = got.text();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    // The Action, its Command, the root, one subdirectory and the input file.
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().all(|line| line["absent"] == true));
    let named =
        |digest: &kbf_types::Digest| lines.iter().any(|l| l["digest"] == digest.to_string());
    assert!(named(&world.input));
    assert!(!named(&world.stranger));

    let refused = request(addr, &farm.pem, "GET", "/locate", None)
        .await
        .expect("connects");
    assert_eq!(refused.status, 401);
    assert_eq!(
        refused.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
}
