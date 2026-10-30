# The image of the task runner (crates/kbf-task): the Ubuntu 24.04 base the farm's Linux actions
# run in, and the `kbf-task` binary the release pipeline built on top of it as one thin layer.
# It holds no installer, no updater and no key.
#
# The release pipeline builds it for each Linux arch (.github/workflows/artifacts.yml) as the
# asset `kbf-task-image-<commit>-<platform>.oci.tar`, from the `kbf-task` it built for the
# commit and checked names it. `cargo xtask pkg-image-asset --service task` refuses the image
# unless /usr/bin/kbf-task is byte for byte that file, and unless the image's layers are the
# base's layers (the committed `base-<platform>.manifest.json`, whose sha256 is the digest
# pinned below) in order plus exactly one layer, which holds that one file. It then records the
# digest of the image manifest and the digest of the base in the facts file
# `kbf-task-image-<commit>-<platform>.facts.json`, which `SHA256SUMS` lists and the release
# attests. A build by hand, for one arch, from a binary built for it:
#
#   cp target/<arch>-unknown-linux-musl/release/kbf-task ctx/
#   docker buildx build --platform linux/<amd64|arm64> -f packaging/container/kbf-task/Dockerfile ctx
#
# The base is docker.io/library/ubuntu, the image tagged `noble-20260917`: an image index with the
# digest sha256:534baea6a22c03a63003dbc8dbe78fe34bc0d7e595d9a9dc9834884ff530eb55. The release
# pins no index: each platform is pinned by the digest of its own manifest in that index.
#
#   linux/amd64  sha256:f610ab94648195aa356059f5b41d6085c9d4d903c072430cdd1af7bdb646106b
#   linux/arm64  sha256:08571ca13e00ca07a2a84eab83a959b4242e22cceb16486a11bef1428c9e93a7
#
# To roll the base, replace both digests below and the two committed manifests (the bytes the
# registry serves for each digest; a test refuses a manifest whose sha256 is not the digest on
# its `FROM` line), and nothing else.
#
# The binary is static (built for the musl target of the arch), so it needs no libc of the image
# it runs in; the base brings glibc, the system CA roots and tzdata for the Commands it runs. The
# image adds no other file, no package and no user.
#
# The user is the numeric 65532:65532, as the frontend and coordinator images run as: a numeric
# user needs no /etc/passwd entry, and adding one would put a second changed layer on the base.
# kbf-task reads no user database; its HOME is a directory it makes.
#
# The copy is the last instruction that adds files, so a new `kbf-task` changes only the top
# layer and leaves the digest of every base layer as the base has it.
FROM --platform=linux/amd64 docker.io/library/ubuntu@sha256:f610ab94648195aa356059f5b41d6085c9d4d903c072430cdd1af7bdb646106b AS base-amd64
FROM --platform=linux/arm64 docker.io/library/ubuntu@sha256:08571ca13e00ca07a2a84eab83a959b4242e22cceb16486a11bef1428c9e93a7 AS base-arm64
FROM base-${TARGETARCH}
COPY --chmod=0755 kbf-task /usr/bin/kbf-task
USER 65532:65532
ENTRYPOINT ["/usr/bin/kbf-task"]
