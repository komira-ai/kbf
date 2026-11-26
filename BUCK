# The workspace through buildrules, a shadow of the Cargo build (docs/farm.md): Cargo stays the
# build of record, and nothing here is read by cargo. Written once by hand; `buildrules sync`
# writes the table it loads (buildrules.lock.bzl, buildrules.bzl, third_party/buildrules/).
#
#   buck2 build //:ci       every member's :fmt, :lint and :test, as the workspace policy in
#                           Cargo.toml and each member's [package.metadata.buildrules] say
#   buck2 build //:check    red unless the table matches the Cargo files (every target runs it too)
#
# No `cargo_sync` target (buildrules' sync as a farm action) is declared: it cannot sync this
# workspace yet, because a member of a git dependency that inherits its own root's
# workspace.dependencies fails to resolve there (komira-ai/buildrules#187). The table is synced
# with the network instead, by farm.yml's stale-table patch (docs/farm.md).
load("//:buildrules.bzl", "cargo_workspace")

cargo_workspace(path = ".")

# The files of the repository outside the crates that a crate's code or tests read (an
# `include_str!`), named in that member's `extra_srcs`. Each is exported by reference: the file
# stays at its path, so the relative path the code uses resolves in the action as in a checkout.
_READ_BY_CRATES = [
    "README.md",
    "docs/deploy/kbf-updater.md",
    "packaging/container/kbf-task",
    "packaging/host-agent/linux/kbf-host-local.service",
    "packaging/linux/units",
]

[
    export_file(
        name = path,
        src = path,
        mode = "reference",
        visibility = ["PUBLIC"],
    )
    for path in _READ_BY_CRATES
]
