//! An Envoy in front of the frontend, as the `box` topology puts one: the frontend is
//! instance `a` (its REAPI, worker and API listeners are the instance's three loopback
//! ports), and the Buck2 client goes through Envoy's REAPI port, never straight to the
//! frontend.
//!
//! Envoy is a host prerequisite, not part of a kbf release: the harness runs the binary
//! it is given (the variable [`ENV_VAR`], as `kbf-deploy`'s validation tests take it).
//! Where there is none, [`find`] says so and the caller records why; CI sets
//! [`REQUIRE_VAR`], which makes absence a failure, so a smoke that never used Envoy
//! cannot go green.
//!
//! The files are `kbf_deploy::envoy::render_box_files`' own render of the box settings
//! built from the farm's ports and certificates, checked with `check_box_files` before
//! Envoy reads them.

use std::path::{Path, PathBuf};

use kbf_deploy::envoy::{self, ENVOY, ENVOY_ARGS};

use crate::procs::Spec;

/// The variable that names the Envoy binary.
pub const ENV_VAR: &str = "KBF_TEST_ENVOY";

/// The variable that makes a missing Envoy a failure (`1`).
pub const REQUIRE_VAR: &str = "KBF_BETA_REQUIRE_ENVOY";

/// The ports of the box: instance `a`'s first loopback port (`b`'s is ten above), and
/// the three Envoy binds.
#[derive(Clone, Copy, Debug)]
pub struct BoxPorts {
    pub base: u16,
    /// Envoy's public REAPI port.
    pub reapi: u16,
    /// Envoy's public worker port.
    pub worker: u16,
    pub admin: u16,
}

/// What [`find`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Binary(PathBuf),
    /// None, and why: for the result document.
    Absent(String),
}

/// The Envoy binary `var` names, if it names a file; else why there is none.
#[must_use]
pub fn find(var: &str) -> Found {
    found(var, std::env::var_os(var))
}

/// [`find`] over the value the variable has (`None` when unset).
#[must_use]
pub fn found(var: &str, value: Option<std::ffi::OsString>) -> Found {
    match value {
        None => Found::Absent(format!("{var} is not set")),
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_file() {
                Found::Binary(path)
            } else {
                Found::Absent(format!("{var}={} is not a file", path.display()))
            }
        }
    }
}

/// Whether `value` (the text of [`REQUIRE_VAR`]) makes Envoy required.
#[must_use]
pub fn required(value: Option<&str>) -> bool {
    value == Some("1")
}

/// The box settings for `ports`, the certificate, key and CA in `etc`.
#[must_use]
pub fn box_settings(ports: &BoxPorts, etc: &Path) -> String {
    let file = |name: &str| etc.join(name).display().to_string();
    format!(
        "KBF_BOX_HOST=localhost\n\
         KBF_BOX_REAPI_PORT={}\n\
         KBF_BOX_WORKER_PORT={}\n\
         KBF_BOX_ADMIN_PORT={}\n\
         KBF_BOX_BACKEND_PORT_BASE={}\n\
         KBF_BOX_CERT_FILE={}\n\
         KBF_BOX_KEY_FILE={}\n\
         KBF_BOX_CA_FILE={}\n\
         KBF_BOX_REAPI_MAX_STREAMS=10000\n\
         KBF_BOX_STREAM_CEILING=65536\n\
         KBF_BOX_LONGEST_ACTION_SECS=3600\n",
        ports.reapi,
        ports.worker,
        ports.admin,
        ports.base,
        file("reapi.pem"),
        file("reapi.key"),
        file("ca.pem"),
    )
}

/// Whether Envoy's admin `/clusters` page (`text`) lists instance `a`'s REAPI and worker
/// endpoints as healthy: the one condition under which it routes to the frontend.
#[must_use]
pub fn healthy(text: &str, ports: &BoxPorts) -> bool {
    let line =
        |cluster: &str, port: u16| format!("{cluster}::127.0.0.1:{port}::health_flags::healthy");
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    lines.contains(&line("kbf_reapi", ports.base).as_str())
        && lines.contains(&line("kbf_workers", ports.base + 1).as_str())
}

/// Renders the box into `dir` (`envoy.json`, `envoy.args`) and checks the render with
/// `kbf-deploy`'s own box check.
///
/// # Errors
/// The settings, the render or the check refused, with the reason.
pub fn render(ports: &BoxPorts, etc: &Path, dir: &Path) -> Result<(), String> {
    let config = dir.join("box.conf");
    std::fs::write(&config, box_settings(ports, etc)).map_err(|e| e.to_string())?;
    envoy::render_box_files(&config, dir).map_err(|e| e.to_string())?;
    check(&config, dir)
}

/// The box check of `kbf-deploy` over the files in `dir`, against the settings in `config`.
///
/// # Errors
/// The files cannot be read, or fail the check (one line per failure).
pub fn check(config: &Path, dir: &Path) -> Result<(), String> {
    let failures = envoy::check_box_files(config, Some(dir)).map_err(|e| e.to_string())?;
    let lines: Vec<String> = failures
        .iter()
        .map(envoy::check::CheckFailure::line)
        .collect();
    if lines.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the box render fails its check:\n{}",
            lines.join("\n")
        ))
    }
}

/// The command that runs Envoy over the box render in `root` (rendered and checked here),
/// its certificates from `etc`.
///
/// # Panics
/// When the render is refused: a defect of the harness.
#[must_use]
pub fn launch(binary: &Path, root: &Path, etc: &Path, ports: &BoxPorts) -> Spec {
    render(ports, etc, root).unwrap_or_else(|e| panic!("{e}"));
    let args = std::fs::read_to_string(root.join(ENVOY_ARGS)).expect("envoy.args");
    let mut argv = vec![
        "--config-path".to_owned(),
        root.join(ENVOY).display().to_string(),
        // One base id per farm, so two farms on a host do not share Envoy's hot-restart
        // memory.
        "--base-id".to_owned(),
        (u32::from(ports.base) * 8 + std::process::id() % 8).to_string(),
        "--concurrency".to_owned(),
        "2".to_owned(),
        "--log-level".to_owned(),
        "warn".to_owned(),
    ];
    argv.extend(args.split_whitespace().map(str::to_owned));
    Spec {
        name: "envoy".to_owned(),
        program: binary.to_owned(),
        argv,
        env: Vec::new(),
        cwd: root.to_owned(),
    }
}

#[cfg(test)]
#[path = "envoy_tests.rs"]
mod tests;
