//! The metrics reader over the Prometheus text the frontend writes, and the scraper's
//! thread over a listener that answers as the operator API does.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;

use super::*;

/// What the frontend's `/metrics` serves, as the Prometheus text format (the families
/// B0 reads, with the HELP and TYPE lines `Families` writes, and a labelled family).
const PAGE: &str = "\
# HELP kbf_execute_errors_total Execute calls that ended in an error status.
# TYPE kbf_execute_errors_total counter
kbf_execute_errors_total 3

# HELP kbf_cache_lost_blobs Blobs whose only copy was lost.
# TYPE kbf_cache_lost_blobs gauge
kbf_cache_lost_blobs 0
kbf_cache_blob_losses_total 12
kbf_storage_member_up{member=\"1\",zone=\"a b\"} 1
kbf_storage_member_up{member=\"2\",zone=\"say \\\"}\\\" {x}\"} 1 1700000000
kbf_tls_reloads_total{listener=\"worker\"} 2.5e1
kbf_gauge_inf +Inf
";

#[test]
fn the_reader_reads_plain_labelled_and_special_values() {
    // Catches: a reader that handles only hand-written pages: a labelled series read
    // with its labels cut at a space, a brace or a quote inside a label value, a trailing
    // timestamp read as the value, an exponent or `+Inf` refused.
    let series = parse(PAGE).expect("a page");
    assert_eq!(series["kbf_execute_errors_total"], 3.0);
    assert_eq!(series["kbf_cache_lost_blobs"], 0.0);
    assert_eq!(series["kbf_cache_blob_losses_total"], 12.0);
    assert_eq!(
        series["kbf_storage_member_up{member=\"1\",zone=\"a b\"}"],
        1.0
    );
    assert_eq!(
        series["kbf_storage_member_up{member=\"2\",zone=\"say \\\"}\\\" {x}\"}"],
        1.0
    );
    assert_eq!(series["kbf_tls_reloads_total{listener=\"worker\"}"], 25.0);
    assert!(series["kbf_gauge_inf"].is_infinite());
    assert_eq!(series.len(), 7);
}

#[test]
fn the_reader_reads_the_frontends_own_page() {
    // Catches: a reader that parses the text of the tests and not what the frontend serves
    // (`tests/golden/frontend-metrics.txt` is its page), and, pinned, the series B0's loss
    // gates read being absent from it: the frontend of a farm holds no cache, so no
    // lost-blob series (`b0`'s known limit; the day the page has them this test is updated
    // with the gates).
    let golden = include_str!("../tests/golden/frontend-metrics.txt");
    let series = parse(golden).expect("the frontend's page");
    let sample = Sample { at: 0.0, series };
    assert_eq!(sample.value(crate::gates::EXECUTE_ERRORS), Some(0.0));
    assert!(sample.value("kbf_execute_requests_total").is_some());
    assert!(sample.value("kbf_storage_health_known").is_some());
    assert_eq!(sample.value(crate::gates::LOST_BLOBS), None);
    assert_eq!(sample.value(crate::gates::BLOB_LOSSES), None);
}

#[test]
fn a_page_that_is_not_metrics_is_an_error() {
    // Catches: an error page (HTML, JSON) read as an empty scrape, which the gates would
    // then call an absent metric: the right gate, but with no word of why.
    assert!(parse("<html>502 Bad Gateway</html>").is_err());
    assert!(parse("kbf_x notanumber").is_err());
    assert!(parse("kbf_x{unclosed 1").is_err());
    assert!(parse("lonely").is_err());
    assert_eq!(parse("").expect("empty"), BTreeMap::new());
    // A `{` that follows the value, not the name, is not a label set.
    assert_eq!(parse("kbf_x 5 {z}").expect("a series")["kbf_x"], 5.0);
}

#[test]
fn a_family_is_the_name_before_its_labels() {
    assert_eq!(family("kbf_x{a=\"b\"}"), "kbf_x");
    assert_eq!(family("kbf_x"), "kbf_x");
}

/// An operator API stand-in on loopback: answers every request with `answer(n)` (n from
/// 0), as an HTTP/1.1 response with a content length, `Connection: close`.
fn serve(answer: impl Fn(usize) -> (u16, String) + Send + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { return };
            // The whole request: its end is a blank line.
            let mut request = Vec::new();
            let mut chunk = [0_u8; 512];
            while !request.ends_with(b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&chunk[..n]),
                }
            }
            let (code, body) = answer(n);
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

#[test]
fn the_scraper_keeps_every_sample_and_counts_the_failures() {
    // Catches: a scraper that stops at its first failure, drops a failed scrape without
    // counting it (the coverage gate would never see it), keeps no last sample after the
    // stop, or writes a file that is not one JSON sample per line.
    let port = serve(|n| match n {
        1 => (503, "unavailable".to_owned()),
        _ => (200, format!("kbf_cache_lost_blobs {n}\n")),
    });
    let dir = crate::scratch(&format!("scrape-{port}"));
    let file = series_file(&dir);
    let scraper = Scraper::start(port, std::time::Duration::from_millis(20), &file);
    // Bounded wait: three good scrapes and the failed one in between.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::fs::read_to_string(&file)
        .unwrap_or_default()
        .lines()
        .count()
        < 3
    {
        assert!(
            std::time::Instant::now() < until,
            "the scraper made no samples"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let scrapes = scraper.stop();
    assert!(scrapes.samples.len() >= 3);
    assert_eq!(scrapes.failed, 1);
    assert_eq!(scrapes.last_error.as_deref(), Some("/metrics answered 503"));
    // The first sample is the first scrape (n = 0); the failed one (n = 1) is not there.
    assert_eq!(scrapes.samples[0].value("kbf_cache_lost_blobs"), Some(0.0));
    assert_eq!(scrapes.samples[1].value("kbf_cache_lost_blobs"), Some(2.0));
    let lines: Vec<String> = std::fs::read_to_string(&file)
        .expect("file")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), scrapes.samples.len());
    let first: serde_json::Value = serde_json::from_str(&lines[0]).expect("JSON");
    assert_eq!(first["series"]["kbf_cache_lost_blobs"], 0.0);
}

#[test]
fn the_last_scrape_is_taken_after_the_stop() {
    // Catches: a scraper that stops with the sample of the last tick as its last, though the
    // scenario's end (the settle, the stop of the farm) came after it: with a tick of a
    // minute the stop comes long before the next one, and the sample taken after it is the
    // second.
    let port = serve(|n| (200, format!("kbf_cache_lost_blobs {n}\n")));
    let dir = crate::scratch(&format!("scrape-last-{port}"));
    let file = series_file(&dir);
    let scraper = Scraper::start(port, std::time::Duration::from_secs(60), &file);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::fs::read_to_string(&file)
        .unwrap_or_default()
        .lines()
        .count()
        < 1
    {
        assert!(
            std::time::Instant::now() < until,
            "the scraper made no sample"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let scrapes = scraper.stop();
    let values: Vec<Option<f64>> = scrapes
        .samples
        .iter()
        .map(|s| s.value("kbf_cache_lost_blobs"))
        .collect();
    assert_eq!(values, [Some(0.0), Some(1.0)]);
    assert_eq!(
        std::fs::read_to_string(&file)
            .expect("file")
            .lines()
            .count(),
        2
    );
}

#[test]
fn a_scraper_with_nothing_to_scrape_counts_failures_and_stops() {
    // Catches: a scraper that hangs on a port nobody answers on, or reports nothing.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let dir = crate::scratch(&format!("scrape-none-{port}"));
    let scraper = Scraper::start(
        port,
        std::time::Duration::from_millis(10),
        &series_file(&dir),
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    let scrapes = scraper.stop();
    assert!(scrapes.samples.is_empty());
    assert!(scrapes.failed >= 1);
    assert_eq!(
        scrapes.last_error.as_deref(),
        Some("no answer from /metrics")
    );
}
