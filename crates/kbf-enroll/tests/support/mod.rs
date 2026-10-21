//! Shared by the integration tests; each test binary uses part of it.
#![allow(dead_code)]

pub mod fake;
pub mod pki;

use kbf_enroll::config::{Config, Host};
use kbf_proto::kbf::enroll::v1::NodeFacts;
use kbf_testkit::dir::TestDir;

/// A fresh scratch directory, removed when it drops (declare it before whatever writes
/// into it). Its path is canonical.
pub fn scratch(name: &str) -> TestDir {
    let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical");
    TestDir::under(temp, &format!("kbf-enroll-it-{name}"))
}

/// A `kbf.toml` for a node that dials `127.0.0.1:port` and keeps its state under `root`.
pub fn config_for(root: &std::path::Path, port: u16, extra: &str) -> Config {
    let text = format!(
        "[frontend]\nurl = \"https://localhost:{port}\"\n[node]\nid = \"node-02\"\n{extra}\n[enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n",
        root.join("enroll.token").display(),
        root.join("state").display(),
    );
    Config::parse(
        &text,
        &Host {
            hostname: "node-02".to_owned(),
            cpus: 8,
        },
    )
    .unwrap_or_else(|e| panic!("config: {e}"))
}

pub fn facts() -> NodeFacts {
    NodeFacts {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        commit: "abc".to_owned(),
        protocol: 1,
        package_version: "0.0.0".to_owned(),
    }
}
