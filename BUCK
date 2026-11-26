# The workspace through buildrules, a shadow of the Cargo build (docs/farm.md): Cargo stays the
# build of record, and nothing here is read by cargo. Written once by hand; `buildrules sync`
# writes the table it loads (buildrules.lock.bzl, buildrules.bzl, third_party/buildrules/).
#
#   buck2 build //:ci             every member's :fmt, :lint and :test, as the workspace policy in
#                                 Cargo.toml and each member's [package.metadata.buildrules] say
#   buck2 build //:sync_check     red unless the committed table is what `buildrules sync` writes now
#   buck2 build //:sync --out .   regenerate the table after a change to a Cargo.toml or Cargo.lock:
#                                 writes the files the sync changed
#
# The two sync targets run `buildrules sync` as a farm action with no network
# (@buildrules//rules/rust:sync.bzl): every crates.io package is downloaded by the sha256 Cargo.lock
# records, every git commit as GitHub's archive by the sha256 and size
# third_party/buildrules/git_archives.tsv records. A git commit the record does not hold yet (a new
# jobscheduler or tierstore pin) is refused by name; farm.yml's stale-table patch adds it.
load("@buildrules//rules/rust:sync.bzl", "cargo_sync")
load("//:buildrules.bzl", "cargo_workspace")
load("//:buildrules.lock.bzl", "PACKAGES")

cargo_workspace(path = ".")

[
    cargo_sync(
        name = name,
        check = check,
        # The channel of rust-toolchain.toml: its pinned cargo and standard library run the sync.
        rustc = "1.95.0",
        # Every member's files (its `:sources`) and the registry and git units, which a glob of
        # this package cannot reach.
        deps = sorted(["//{}:sources".format(p["path"]) for p in PACKAGES.values()]) + ["//third_party/buildrules:files"],
        srcs = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "BUCK", "buildrules.bzl", "buildrules.lock.bzl"] + glob(["toolchains/**"]),
        visibility = ["PUBLIC"],
    )
    for name, check in [("sync", False), ("sync_check", True)]
]
