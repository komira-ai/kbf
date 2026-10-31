//! `credential_files`: the files the Command must not read.

use std::path::PathBuf;

use clap::Parser as _;
use kbf_segments::sha256;

use super::*;

fn parse(args: &[&str]) -> Cli {
    let action = format!("--action={}", sha256(b"a"));
    let mut all = vec!["kbf-task", "--grant-url=g", "--lease=l", action.as_str()];
    all.extend_from_slice(args);
    Cli::try_parse_from(all).expect("flags")
}

/// Catches a credential file the Command could read (macOS denies each in its sandbox profile): the
/// grant token file, the pack URL file and the PUT URLs file are named, in that order, and a root
/// certificate file, which holds no credential, and a credential given as a flag value are not.
#[test]
fn the_files_holding_a_credential_are_the_ones_the_flags_name() {
    let all = parse(&[
        "--put-urls-file=/run/puts",
        "--grant-token-file=/run/grant",
        "--root-der=/etc/root.der",
    ]);
    assert_eq!(
        credential_files(&all),
        [PathBuf::from("/run/grant"), PathBuf::from("/run/puts")]
    );
    let pack = parse(&["--pack-url-file=/run/url", "--grant-token-file=/run/grant"]);
    assert_eq!(
        credential_files(&pack),
        [PathBuf::from("/run/grant"), PathBuf::from("/run/url")]
    );
    let by_value = parse(&["--pack-url=u", "--grant-token=t", "--insecure-argv-secrets"]);
    assert!(credential_files(&by_value).is_empty());
}
