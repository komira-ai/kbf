//! The hook for comparing a render with the files of a host: ignored by default.
//!
//! Put the settings file the host was installed with in `tests/slice1/site.conf`, and in
//! `tests/slice1/installed.sha256` one line per installed file, in the order
//! `render` then `render_tls` give them (the slice, the five services, the Caddyfile,
//! the certificate job, its timer, the reload path and service):
//! `<sha256 of the file without its comment lines>  <file name>`, which is what
//! `grep -v '^[[:space:]]*#' FILE | sha256sum` prints with the name added. For a host
//! kbf-deploy installed, compute the slice's hash after
//! `sed 's/ on the dev box//'`. Then run
//! `cargo test -p mdm-core --test installed_host -- --ignored`.
//!
//! The test fails, naming the files whose hashes differ, when the host and this crate's
//! render disagree. Nothing else depends on it.

mod common;

use common::read;
use mdm_core::acme::Acme;
use mdm_core::config::MdmConfig;
use mdm_core::render::{Comments, manifest, render, render_tls};

/// The installed host's files hash as this crate's render of the host's settings does.
#[test]
#[ignore = "needs tests/slice1/site.conf and tests/slice1/installed.sha256 from a host"]
fn the_installed_files_match_the_render() {
    let site = read("slice1/site.conf");
    let installed = read("slice1/installed.sha256");
    let c = MdmConfig::parse(&site).expect("the host's settings are valid");
    let acme = Acme::parse(&site).expect("the host's ACME settings are valid");
    let mut files = render(&c).expect("renders").files();
    files.extend(render_tls(&c, &acme).expect("renders"));
    let ours = manifest(&files, Comments::Strip);
    let differ: Vec<String> = ours
        .lines()
        .zip(installed.lines())
        .filter(|(a, b)| a != b)
        .map(|(a, _)| a.to_owned())
        .collect();
    assert!(differ.is_empty(), "differ: {differ:#?}");
    assert_eq!(ours.lines().count(), installed.lines().count());
}
