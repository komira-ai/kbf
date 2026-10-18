# The kbf frontend image: the release `kbf` binary and nothing else.
#
# Built by the `frontend-image` job in .github/workflows/artifacts.yml, from the
# linux-x86_64 `kbf` tarball that job checks against SHA256SUMS (the bytes attest-linux
# signs on a push). The build context is this directory with that binary extracted next to
# this file as `kbf`. No package manager, no shell, no Envoy: Envoy is its own Deployment
# (deploy/k8s/envoy-deployment.yaml).
#
# The base is distroless `cc` (glibc and libgcc, the C++ runtime), the `nonroot` tag,
# pinned by the digest of the multi-arch index. The release binary is built on
# ubuntu-24.04 (glibc 2.39); this base carries a newer glibc. Bump the digest on purpose.
FROM gcr.io/distroless/cc-debian13@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2

COPY --chmod=0555 kbf /usr/local/bin/kbf

# Not root, as the base's `nonroot` user. A pod sets runAsUser itself (the two frontend
# instances must share one uid: the handover socket checks the peer's uid).
USER 65532:65532

# No default mode: `kbf` without a mode word refuses to start, and the Deployment gives it.
ENTRYPOINT ["/usr/local/bin/kbf"]
