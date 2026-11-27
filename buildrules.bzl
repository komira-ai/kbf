# Written by `buildrules sync`. Do not edit: `buildrules check` fails when this differs from what sync writes.
# Binds the generated table (buildrules.lock.bzl) to the two macros of @buildrules//rules/rust:defs.bzl.
load("@buildrules//rules/rust:defs.bzl", _cargo_package = "cargo_package", _cargo_workspace = "cargo_workspace")
load(":buildrules.lock.bzl", "CARGO_LOCK_SHA256", "MANIFEST_SHA256", "MEMBERS", "PACKAGES", "RUSTC", "TOOLCHAIN", "UNITS", "WORKSPACE")

THIRD_PARTY = "//third_party/buildrules"

CARGO = {
    "CARGO_LOCK_SHA256": CARGO_LOCK_SHA256,
    "MANIFEST_SHA256": MANIFEST_SHA256,
    "MEMBERS": MEMBERS,
    "PACKAGES": PACKAGES,
    "RUSTC": RUSTC,
    "TOOLCHAIN": TOOLCHAIN,
    "UNITS": UNITS,
    "WORKSPACE": WORKSPACE,
}

def cargo_package(path = ".", **kwargs):
    _cargo_package(path = path, table = CARGO, third_party = THIRD_PARTY, **kwargs)

def cargo_workspace(path = ".", **kwargs):
    _cargo_workspace(path = path, table = CARGO, **kwargs)
