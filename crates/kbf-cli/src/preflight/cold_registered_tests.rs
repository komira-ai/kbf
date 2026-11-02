//! The check of a storage node on `--cold-store registered`: its key pair comes from the
//! frontend's registration, so the host is asked for no key pair at all, neither the
//! environment's nor a file's. Each test goes from the flags to the needs.

use super::tests::needs_from;

const REGISTERED: &str = "--cold-store=registered";
const FILE: &str = "/etc/kbf/cold.json";

fn registered(extra: &[&str]) -> super::Needs {
    let mut args = vec![
        "storage",
        REGISTERED,
        "--meta-dir=/m",
        "--frontend=http://127.0.0.1:8981",
    ];
    args.extend_from_slice(extra);
    needs_from(&args)
}

fn key_files(needs: &super::Needs) -> usize {
    needs
        .files
        .iter()
        .filter(|f| f.flag == "--cold-store-credentials-file")
        .count()
}

/// Pins that a registered node is asked for no `AWS_*` pair and no key file (defect: the
/// store counted as an S3 store, whose requests the environment's pair signs).
#[test]
fn a_registered_node_needs_no_key_pair_from_the_host() {
    let needs = registered(&[]);
    assert!(!needs.s3, "the environment's pair is not needed");
    assert_eq!(key_files(&needs), 0);
}

/// Catches a preflight that checks a key file the start refuses (defect: the file checked as
/// for an `s3://` store): with `--cold-store-credentials-file` beside `registered` the report
/// would say the file is missing or open, ahead of the start's own refusal, which says the
/// flag cannot be used with `registered`. The file is not among the files needed, and the
/// environment is still not asked for.
#[test]
fn a_key_file_beside_registered_is_the_starts_to_refuse_not_the_checks_to_read() {
    let flag = format!("--cold-store-credentials-file={FILE}");
    let needs = registered(&[&flag]);
    assert_eq!(key_files(&needs), 0, "the file is not read");
    assert!(!needs.s3);
}
