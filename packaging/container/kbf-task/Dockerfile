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
#   cp target/release/kbf-task ctx/
#   docker buildx build --platform linux/<amd64|arm64> -f packaging/container/kbf-task/Dockerfile ctx
#
# The base is the coordinator's (packaging/container/kbf-coordinator/Dockerfile): distroless `cc`
# on Debian 13, pinned by its image index digest, with no shell and no package manager. Its
# `nonroot` user, uid 65532, is the user below. The image declares no port and no default
# command: what the task is given is the entrypoint's arguments.
#
# The image holds no shell, compiler or other program, so it runs only a Command that is
# self-contained (a static binary it brings in its input tree). A Command that needs a shell or a
# toolchain runs in an image that has them, with this `kbf-task` added to it; that image is not
# built here.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e
COPY --chmod=0755 kbf-task /usr/bin/kbf-task
USER 65532:65532
ENTRYPOINT ["/usr/bin/kbf-task"]
