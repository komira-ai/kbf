# The image of the kbf frontend (docs/design/distribution.md): the `kbf` binary the release
# pipeline built and attested, and nothing else. It is never a rebuild: the pipeline unpacks
# `kbf` from `kbf-<commit>-linux-<arch>.tar.gz` into the build context, and `cargo xtask
# pkg-image-asset` refuses the image unless /usr/bin/kbf is byte for byte that file.
#
# The release pipeline builds it for each Linux arch (.github/workflows/artifacts.yml) as the
# asset `kbf-frontend-image-<commit>-<platform>.oci.tar`; a build by hand, for one arch:
#
#   tar -xzf kbf-<commit>-linux-<arch>.tar.gz -C ctx kbf
#   docker buildx build --platform linux/<amd64|arm64> -f packaging/container/frontend/Dockerfile ctx
#
# The base is distroless `cc` on Debian 13, pinned by its image index digest (one digest for
# both arches). The binary is a glibc build: it links the C library and libgcc, which `cc` holds
# (Debian 13 has glibc 2.41, newer than the 2.39 of the runner that builds kbf). The base holds
# the CA roots the frontend's https clients read (the object store and the OIDC key fetch),
# no shell and no package manager, so there is nothing to patch in the image but the base itself,
# and the image does not depend on a package archive at build time (a build is reproducible).
# The base's `nonroot` user, uid 65532, is the user below.
#
# Files and directories the orchestrator mounts (every path is kbf.toml's to name):
#   /etc/kbf/kbf.toml   the configuration, as the kbf-frontend unit's --config names it
#   the certificates, the state store and the cold store kbf.toml names, on volumes the
#   user below (uid 65532) may read and write. The image holds none of them and no secret.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e
COPY --chmod=0755 kbf /usr/bin/kbf
USER 65532:65532
# REAPI (--listen), the worker listener (--worker-listen) and the operator API (--api-listen)
# by default (docs/deploy/kbf-modes.md). The listen addresses are kbf.toml's.
EXPOSE 8980 8981 8982
ENTRYPOINT ["/usr/bin/kbf", "--mode=frontend"]
CMD ["--config", "/etc/kbf/kbf.toml"]
