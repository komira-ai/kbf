//! [`problems`] on the real files with one change made, so each rule refuses it.

use super::*;

/// The problems of the real files with every `from` in the workflow `file` replaced by `to`.
/// The change must alter the text.
fn changed_to(file: &str, from: &str, to: &str) -> Vec<String> {
    let (mut workflows, pins) = real_files();
    let text = &mut workflows
        .iter_mut()
        .find(|(n, _)| n == file)
        .unwrap_or_else(|| panic!("no {file}"))
        .1;
    let changed = text.replace(from, to);
    assert_ne!(&changed, text, "{file}: `{from}` is not in the file");
    *text = changed;
    problems(&workflows, &pins)
}

/// Some line of `found` holds `part`.
fn has(found: &[String], part: &str) {
    assert!(
        found.iter().any(|f| f.contains(part)),
        "want {part:?} in {found:#?}"
    );
}

/// The ci.yml lint job's fetch step.
const FETCH: &str =
    "cargo xtask debs --pins .github/debs/musl-tools.toml --dir \"$RUNNER_TEMP/debs/musl-tools\"";
/// The ci.yml lint job's install step.
const INSTALL: &str = "sudo dpkg --install --recursive \"$RUNNER_TEMP/debs/musl-tools\"";
/// The update-rights-linux check of the image's packages.
const QUERY: &str = "dpkg-query --show nftables polkitd";

/// Catches apt run under any spelling: plain, behind `sudo` with options, `env`, an
/// assignment, or as a later command of a chain or pipe.
#[test]
fn apt_in_any_command_of_a_run_line_is_refused() {
    for line in [
        "sudo apt-get install -y nftables",
        "apt install nftables",
        "sudo -E aptitude install nftables",
        "env DEBIAN_FRONTEND=noninteractive apt-get install nftables",
        "DEBIAN_FRONTEND=noninteractive sudo apt-get update",
        "true && sudo apt-get update",
        "true || apt-get update",
        "true; apt-get update",
        "yes | apt-get install nftables",
        "sudo -u root apt-get install nftables",
        "sudo -E -u root env A=1 apt-get update",
    ] {
        let found = changed_to("update-rights-linux.yml", QUERY, line);
        has(&found, "update-rights-linux.yml: root: step 4: runs apt");
    }
}

/// Catches a run line naming a Canonical host, archive or PPA.
#[test]
fn a_canonical_host_is_refused() {
    for (host, url) in [
        (
            "ubuntu.com",
            "curl -fsSLO https://azure.archive.ubuntu.com/ubuntu/pool/x.deb",
        ),
        ("launchpad.net", "curl -fsSLO https://launchpad.net/x"),
        ("canonical.com", "curl -fsSLO https://canonical.com/x"),
    ] {
        let found = changed_to("update-rights-linux.yml", QUERY, url);
        has(
            &found,
            &format!("update-rights-linux.yml: root: step 4: names the Canonical host {host}"),
        );
    }
}

/// Catches a dpkg install outside the four debs steps, in either spelling.
#[test]
fn a_dpkg_install_elsewhere_is_refused() {
    for line in [
        "sudo dpkg --install x.deb",
        "sudo dpkg -i x.deb",
        "dpkg -iR dir",
        "dpkg --unpack x.deb",
    ] {
        let found = changed_to("update-rights-linux.yml", QUERY, line);
        has(
            &found,
            "update-rights-linux.yml: root: step 4: a dpkg install that is not the last of the four debs steps",
        );
    }
}

/// Catches each way the four debs steps stop binding the pins: the fetch not one command, its
/// flags or set wrong, no fetch id; the restore missing, under another `if:`, without an id,
/// with `restore-keys`, another path or a key that is not the pins file's hash; the save
/// missing, run on a hit or after a failed fetch, saving another key; the install not dpkg of
/// the set's directory or under another `if:`.
#[test]
fn each_drift_of_the_four_steps_is_refused() {
    let ci = |from: &str, to: &str| changed_to("ci.yml", from, to);
    let cases: Vec<(Vec<String>, &str)> = vec![
        (
            ci(FETCH, &format!("{FETCH} && true")),
            "the debs step is not one command (\"&&\" in it)",
        ),
        (
            ci(FETCH, &FETCH.replace("--pins", "--pin")),
            "the debs step is not `cargo xtask debs --pins <file> --dir <dir>`",
        ),
        (
            ci(
                FETCH,
                &FETCH.replace(".github/debs/musl-tools.toml", "musl.toml"),
            ),
            "the --pins \"musl.toml\" is not .github/debs/<set>.toml",
        ),
        (
            ci(
                FETCH,
                &FETCH.replace("$RUNNER_TEMP/debs/musl-tools", "/tmp/debs"),
            ),
            "the --dir \"/tmp/debs\" is not \"$RUNNER_TEMP/debs/musl-tools\"",
        ),
        (
            ci("        id: musl-debs-fetch\n", ""),
            "the debs step has no `id:`",
        ),
        (
            ci(
                "uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n        id: musl-debs\n",
                "uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        id: musl-debs\n",
            ),
            "the step before the debs step is not actions/cache/restore",
        ),
        (
            ci(
                "      - name: Restore the musl-tools packages\n        if: ${{ !cancelled() }}",
                "      - name: Restore the musl-tools packages\n        if: ${{ always() }}",
            ),
            "the restore's `if:` is Some(\"always()\"), not the debs step's Some(\"!cancelled()\")",
        ),
        (
            ci("        id: musl-debs\n", ""),
            "the restore has no `id:`",
        ),
        (
            ci(
                "          key: debs-musl-tools-",
                "          restore-keys: debs-\n          key: debs-musl-tools-",
            ),
            "the restore's `with:` has [\"key\", \"path\", \"restore-keys\"], not exactly path and key",
        ),
        (
            ci(
                "key: debs-musl-tools-${{ runner.arch }}-${{ hashFiles('.github/debs/musl-tools.toml') }}",
                "key: debs-musl-tools-${{ runner.arch }}",
            ),
            "the restore's key is Some(\"debs-musl-tools-${{ runner.arch }}\")",
        ),
        (
            ci(
                "uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0",
                "uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1",
            ),
            "the step after the debs step is not actions/cache/save",
        ),
        (
            ci(" && steps.musl-debs.outputs.cache-hit != 'true'", ""),
            "the save's `if:` is Some(\"!cancelled() && steps.musl-debs-fetch.outcome == 'success'\")",
        ),
        (
            ci("steps.musl-debs-fetch.outcome == 'success' && ", ""),
            "the save's `if:` is Some(\"!cancelled() && steps.musl-debs.outputs.cache-hit != 'true'\")",
        ),
        (
            ci(
                "key: ${{ steps.musl-debs.outputs.cache-primary-key }}",
                "key: debs-musl-tools",
            ),
            "the save's key is Some(\"debs-musl-tools\")",
        ),
        (
            ci(
                "          path: ${{ runner.temp }}/debs/musl-tools\n          key: ${{ steps",
                "          path: ${{ runner.temp }}/debs\n          key: ${{ steps",
            ),
            "the save's path is Some(\"${{ runner.temp }}/debs\")",
        ),
        (
            ci(
                INSTALL,
                "sudo dpkg --install \"$RUNNER_TEMP\"/debs/musl-tools/*.deb",
            ),
            "the step after the save is not `sudo dpkg --install --recursive \"$RUNNER_TEMP/debs/musl-tools\"`",
        ),
        (
            ci(
                &format!("        if: ${{{{ !cancelled() }}}}\n        run: {INSTALL}"),
                &format!("        run: {INSTALL}"),
            ),
            "the step after the save is not",
        ),
    ];
    for (found, want) in cases {
        has(&found, "ci.yml: lint: step ");
        has(&found, want);
    }
}

/// Catches a debs step with no step before or after it to cache the set.
#[test]
fn a_debs_step_at_either_end_of_a_job_is_refused() {
    let first = "jobs:\n  j:\n    runs-on: ubuntu-24.04\n    steps:\n      - id: f\n        run: cargo xtask debs --pins .github/debs/musl-tools.toml --dir \"$RUNNER_TEMP/debs/musl-tools\"\n";
    let (sites, found) = scan(&[("x.yml".to_owned(), first.to_owned())]);
    assert_eq!(sites, []);
    assert_eq!(
        found,
        ["x.yml: j: step 1: no actions/cache/restore step right before the debs step"]
    );
    let last = format!(
        "jobs:\n  j:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9\n        id: r\n        with:\n          path: ${{{{ runner.temp }}}}/debs/musl-tools\n          key: debs-musl-tools-${{{{ runner.arch }}}}-${{{{ hashFiles('.github/debs/musl-tools.toml') }}}}\n      - id: f\n        run: {FETCH}\n"
    );
    let (_, found) = scan(&[("x.yml".to_owned(), last.clone())]);
    assert_eq!(
        found,
        ["x.yml: j: step 2: no actions/cache/save step right after the debs step"]
    );
    let no_install = format!(
        "{last}      - if: ${{{{ steps.f.outcome == 'success' && steps.r.outputs.cache-hit != 'true' }}}}\n        uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9\n        with:\n          path: ${{{{ runner.temp }}}}/debs/musl-tools\n          key: ${{{{ steps.r.outputs.cache-primary-key }}}}\n"
    );
    let (_, found) = scan(&[("x.yml".to_owned(), no_install.clone())]);
    assert_eq!(
        found,
        ["x.yml: j: step 2: no dpkg install step right after the save"]
    );
    let whole = format!("{no_install}      - run: {INSTALL}\n");
    let (sites, found) = scan(&[("x.yml".to_owned(), whole)]);
    assert_eq!(found, Vec::<String>::new());
    assert_eq!(
        sites,
        [Site {
            file: "x.yml".to_owned(),
            job: "j".to_owned(),
            set: "musl-tools".to_owned(),
        }]
    );
}

/// Catches a pins file no workflow installs, a site whose pins file is gone, and a workflow
/// that does not parse being skipped instead of reported.
#[test]
fn an_orphan_pins_file_a_missing_one_and_an_unreadable_workflow_are_refused() {
    let (workflows, mut pins) = real_files();
    pins.push(".github/debs/other.toml".to_owned());
    assert_eq!(
        problems(&workflows, &pins),
        [".github/debs/other.toml: no workflow installs this set"]
    );
    let found = problems(&workflows, &[]);
    for job in [
        "ci.yml: lint",
        "artifacts.yml: linux-x86_64",
        "artifacts.yml: linux-arm64",
    ] {
        has(
            &found,
            &format!("{job}: the pins file .github/debs/musl-tools.toml does not exist"),
        );
    }
    let (_, found) = scan(&[("x.yml".to_owned(), "jobs: [".to_owned())]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("x.yml: "), "{found:?}");
    // A job with no steps list that calls no reusable workflow is reported; one that calls
    // one is not.
    let (_, found) = scan(&[(
        "x.yml".to_owned(),
        "jobs:\n  j:\n    runs-on: ubuntu-24.04\n  k:\n    uses: ./.github/workflows/ci.yml\n"
            .to_owned(),
    )]);
    assert_eq!(found, ["x.yml: j: the job `j` has no steps list"]);
}

/// Asserts the command splitting the apt rule reads by: chains and pipes split, `sudo` and
/// its options, `env` and assignments dropped, quotes stripped.
#[test]
fn commands_are_read_after_sudo_env_and_assignments() {
    assert_eq!(
        commands("sudo -E env A=1 \"apt-get\" update && B_2=x dpkg -i a | tee log"),
        [
            vec!["apt-get".to_owned(), "update".to_owned()],
            vec!["dpkg".to_owned(), "-i".to_owned(), "a".to_owned()],
            vec!["tee".to_owned(), "log".to_owned()],
        ]
    );
    assert_eq!(
        commands("=x apt-get"),
        [vec!["=x".to_owned(), "apt-get".to_owned()]]
    );
    assert_eq!(
        commands("sudo -u root apt-get update"),
        [vec!["apt-get".to_owned(), "update".to_owned()]]
    );
    assert_eq!(commands("sudo -u"), [Vec::<String>::new()]);
}
