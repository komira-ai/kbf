# Captured buck2 event logs

Each `*.json-lines.gz` here is a file buck2 wrote itself (`buck2 build --event-log
FILE.json-lines.gz`), copied unchanged from the logs a CI job uploaded; nothing here
is written by hand. Each `*.console.log` is the same build's console (`--console
simple`), whose summary line the tests check the counts against.

The buck2 is the release CI pins (`BUCK2_VERSION` 2026-10-01 in
`.github/workflows/ci.yml` and `integration.yml`; the logs name its revision,
`2026-09-30-2484d70a6f8a5b1f26cbdfdbd7758c9c5b746f49`).

| file | job | build |
|---|---|---|
| `m1-first` | `integration / m1-cell` | `crates/kbf-it/m1/buck2`, `//...` on a fresh cell: 5 actions run on the farm |
| `m1-second` | `integration / m1-cell` | the same after `buck2 clean`: 5 remote cache hits |
| `m1-failing` | `integration / m1-cell` | `//...` with `-c kbf_m1.fail=yes`: `//:verdict` fails (exit 3) on the farm, `//:pause` is a cache hit |
| `m2-first` | `ci / m2-container` | `crates/kbf-it/m2/buck2`, `//...` through the container driver: 5 actions on the farm, `//:hog` run again after its memory kill |

Captured by workflow runs 38245718952 (integration) and 38245719045 (ci) of
komira-build-farm, on commit 0718a2d, which added the event logs to `crates/kbf-it/m1/run.sh`
and `crates/kbf-it/m2/run.sh`. To capture again, download the `m1-cell-logs` or
`m2-container-logs` artifact of a run and copy `buck2-<name>.json-lines.gz` and
`buck2-<name>.log` here. The logs hold the hosted runner's paths and host name, as
buck2 records them.
