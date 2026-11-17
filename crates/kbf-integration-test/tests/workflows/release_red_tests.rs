//! Tests of the `issues: write` condition of the `release-red` job of suites.yml. Each says which defect it catches.

use super::tests::{SIGNING_PUSH_IF, clean, refused, workflow};

/// The `if:` line of the `release-red` job of suites.yml.
const RELEASE_RED_IF: &str = "    if: ${{ always() && github.ref == 'refs/heads/main' }}\n";

#[test]
fn issues_write_is_allowed_for_the_release_red_condition_and_no_look_alike() {
    // Catches: the job that keeps the RELEASE-RED issue refused (its issue would never open),
    // `always()` alone (a run for any ref, a pull request included, holding a token that writes
    // issues), the ref test spelled another way, `always()` joined by `||` or placed after the
    // ref test, and the pipeline's own push condition unlocking `issues`.
    clean(&workflow(&format!(
        "    runs-on: ubuntu-latest\n{RELEASE_RED_IF}    permissions: {{contents: read, actions: read, issues: write}}\n"
    )));
    for cond in [
        "    if: ${{ always() }}\n",
        "    if: ${{ always() || github.ref == 'refs/heads/main' }}\n",
        "    if: ${{ github.ref == 'refs/heads/main' && always() }}\n",
        "    if: ${{ always() && github.ref != 'refs/heads/main' }}\n",
        "    if: ${{ always() && github.ref == 'refs/heads/dev' }}\n",
        "    if: ${{ always() && startsWith(github.ref, 'refs/heads/') }}\n",
        "    if: ${{ always() && github.event_name != 'pull_request' }}\n",
        "    if: ${{ !cancelled() && github.ref == 'refs/heads/main' }}\n",
        "    if: always() && github.ref == 'refs/heads/main'\n",
        SIGNING_PUSH_IF,
    ] {
        refused(
            &format!("    runs-on: ubuntu-latest\n{cond}    permissions: {{issues: write}}\n"),
            "`issues: write` is allowed only in a job whose `if:` is exactly",
        );
    }
}

#[test]
fn the_release_red_condition_unlocks_no_other_write_scope() {
    // Catches: the suites exception read as a wider grant: with the release-red `if:`, `contents`,
    // the signing scopes, `packages` and `actions` still may not be `write`, and `issues` may not
    // be another level.
    for scope in ["id-token", "attestations", "contents", "actions"] {
        refused(
            &format!(
                "    runs-on: ubuntu-latest\n{RELEASE_RED_IF}    permissions: {{{scope}: write}}\n"
            ),
            &format!("`{scope}: write` is allowed only in a job whose `if:` is exactly"),
        );
    }
    for p in ["{packages: write}", "{issues: admin}", "write-all"] {
        refused(
            &format!("    runs-on: ubuntu-latest\n{RELEASE_RED_IF}    permissions: {p}\n"),
            "a job's `permissions`",
        );
    }
}
