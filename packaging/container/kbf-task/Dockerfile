# The image of the task runner (crates/kbf-task): the `kbf-task` binary the release pipeline
# built, and nothing else. It is a runner for one task: it holds no installer, no updater and no
# key.
#
# The release pipeline builds it for each Linux arch (.github/workflows/artifacts.yml) as the
# asset `kbf-task-image-<commit>-<platform>.oci.tar`, from the `kbf-task` it built for the
# commit and checked names it. `cargo xtask pkg-image-asset --service task` refuses the image
# unless /usr/bin/kbf-task is byte for byte that file. A build by hand, for one arch, from a
# binary built for it:
#
#   cp target/<arch>-unknown-linux-musl/release/kbf-task ctx/
#   docker buildx build --platform linux/<amd64|arm64> -f packaging/container/kbf-task/Dockerfile ctx
#
# The binary is static (built for the musl target of the arch, so it needs no libc of the image
# it runs in). The base is distroless `static` on Debian 13, pinned by its image index digest:
# the system CA roots and the `nonroot` user, uid 65532 (the user below), with no libc, no shell
# and no package manager. The image declares no port and no default
# command: what the task is given is the entrypoint's arguments.
#
# The image holds no shell, compiler or other program, so it runs only a Command that is
# self-contained (a static binary it brings in its input tree). A Command that needs a shell or a
# toolchain runs in an image that has them, with this `kbf-task` added to it; that image is not
# built here.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:18f4cc26c9ffa7f20a359298be7deadac5c607c8e03a02b8119f1a5fc2f82c99
COPY --chmod=0755 kbf-task /usr/bin/kbf-task
USER 65532:65532
ENTRYPOINT ["/usr/bin/kbf-task"]
