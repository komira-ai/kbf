//! The box render the harness builds from a farm's ports and certificates.

use super::*;

fn ports() -> BoxPorts {
    BoxPorts {
        base: 12_340,
        reapi: 12_400,
        worker: 12_401,
        admin: 12_402,
    }
}

fn dir(name: &str) -> PathBuf {
    crate::scratch(&format!("envoy-{name}"))
}

#[test]
fn the_box_render_passes_the_box_check() {
    // Catches: a `BoxConfig` the harness builds that kbf-deploy's own box check would
    // refuse (a port wrong, a file missing), which Envoy would then not serve as the box does.
    let d = dir("render");
    render(&ports(), Path::new("/etc/farm"), &d).expect("a render the check passes");
    let json = std::fs::read_to_string(d.join(ENVOY)).expect("envoy.json");
    assert!(json.contains("12340"), "instance a's REAPI port");
    assert!(json.contains("12350"), "instance b's REAPI port, ten above");
    assert!(json.contains("/etc/farm/reapi.pem"));
    assert!(d.join(ENVOY_ARGS).exists());
}

#[test]
fn a_port_used_twice_is_refused() {
    // Catches: a farm whose Envoy port is also an instance's, which the render would
    // accept as two listeners on one port and fail at bind.
    let d = dir("twice");
    let mut p = ports();
    p.admin = p.base + 2;
    let err = render(&p, Path::new("/etc/farm"), &d).expect_err("refused");
    assert!(err.contains("both port"), "{err}");
}

#[test]
fn a_render_that_fails_the_check_is_refused_with_its_failures() {
    // Catches: a check that cannot fail (a render edited after it was made, as a hand edit
    // of `envoy.json` would, passing): the check must read the files and name what is wrong.
    let d = dir("tampered");
    render(&ports(), Path::new("/etc/farm"), &d).expect("a render the check passes");
    let json = std::fs::read_to_string(d.join(ENVOY)).expect("envoy.json");
    std::fs::write(d.join(ENVOY), json.replace("kbf_reapi", "kbf_elsewhere")).expect("edit");
    let err = check(&d.join("box.conf"), &d).expect_err("the edit is caught");
    assert!(
        err.starts_with("the box render fails its check:\nFAIL "),
        "{err}"
    );
    // And a file that is not there is an error of its own, not a pass.
    std::fs::remove_file(d.join(ENVOY)).expect("remove");
    assert!(check(&d.join("box.conf"), &d).is_err());
}

#[test]
fn the_healthy_page_names_both_instances_endpoints() {
    // Catches: a farm called routable while Envoy still has the frontend as unhealthy on one
    // of its two clusters (the page lists the endpoint without `healthy`), or while its
    // endpoint is another instance's.
    let p = ports();
    let page = |a: &str, b: &str| {
        format!(
            "kbf_reapi::127.0.0.1:{}::health_flags::{a}\nkbf_workers::127.0.0.1:{}::health_flags::{b}\n",
            p.base,
            p.base + 1
        )
    };
    assert!(healthy(&page("healthy", "healthy"), &p));
    assert!(!healthy(&page("healthy", "/failed_active_hc"), &p));
    assert!(!healthy(&page("/failed_active_hc", "healthy"), &p));
    assert!(!healthy(
        &page("healthy", "healthy").replace(&p.base.to_string(), "1"),
        &p
    ));
    assert!(!healthy("", &p));
}

#[test]
fn the_launch_runs_envoy_over_the_render() {
    let d = dir("launch");
    let spec = launch(
        Path::new("/opt/envoy"),
        &d,
        Path::new("/etc/farm"),
        &ports(),
        7,
    );
    assert_eq!(spec.program, PathBuf::from("/opt/envoy"));
    // Catches: two farms sharing Envoy's shared-memory region (the base id is the one given,
    // which is the farm's block number), and an Envoy that hot restarts and leaves files in
    // `/dev/shm`. Mutant: the id is derived from the process id again, or the flag is dropped.
    let at = spec.argv.iter().position(|a| a == "--base-id").expect("id");
    assert_eq!(spec.argv[at + 1], "7");
    assert!(spec.argv.contains(&"--disable-hot-restart".to_owned()));
    let at = spec
        .argv
        .iter()
        .position(|a| a == "--config-path")
        .expect("the config");
    assert_eq!(spec.argv[at + 1], d.join(ENVOY).display().to_string());
    // The hot-restart flags of the render come after Envoy's own.
    assert!(spec.argv.contains(&"--parent-shutdown-time-s".to_owned()));
    assert!(
        spec.argv
            .contains(&"--skip-hot-restart-on-no-parent".to_owned())
    );
}

#[test]
fn the_binary_is_found_by_its_variable() {
    // Catches: a path that is not a file taken for an Envoy, an unset variable taken for
    // one that is set, and a file not found.
    assert_eq!(
        find("KBF_TEST_NO_SUCH_VARIABLE"),
        Found::Absent("KBF_TEST_NO_SUCH_VARIABLE is not set".to_owned())
    );
    let d = dir("find");
    let Found::Absent(why) = found("V", Some(d.clone().into_os_string())) else {
        panic!("a directory is not a binary");
    };
    assert!(why.ends_with("is not a file"), "{why}");
    let binary = d.join("envoy");
    std::fs::write(&binary, b"").expect("file");
    assert_eq!(
        found("V", Some(binary.clone().into_os_string())),
        Found::Binary(binary)
    );
}

#[test]
fn only_one_requires_envoy() {
    // Catches: any other text (`0`, `true`, empty) switching the requirement on, or `1`
    // not doing so: CI sets `1`, and a skip in CI would be a smoke that never used Envoy.
    assert!(required(Some("1")));
    for other in [None, Some(""), Some("0"), Some("true")] {
        assert!(!required(other), "{other:?}");
    }
}
