# The image of the frontend (docs/deploy/containers.md): the `kbf` binary, run as `kbf
# --mode=frontend`, and nothing else. No installer, no updater, no proxy and no Envoy: the
# frontend runs behind the orchestrator's own proxy or load balancer (docs/design/distribution.md,
# section 2). Nothing secret is in the image: the configuration, the certificates, the keys and
# the operator token are files the orchestrator mounts.
#
# The release workflow builds this image for each Linux platform from the `kbf` it attests
# and ships it as `kbf-frontend-image-<commit>-<platform>.tar` (docs/artifacts.md). To build
# one from a local binary, put the binary, built for the image's platform, in the build context:
#
#   cargo build --release --locked -p kbf-cli --bin kbf
#   cp target/release/kbf packaging/container/kbf-frontend/
#   docker build --build-arg COMMIT=<the commit> packaging/container/kbf-frontend
#
# The base is pinned by its image index digest, as the images of the package install tests are
# (packaging/linux/test).
#
# Files the orchestrator mounts:
#   /etc/kbf/kbf.toml          the settings of the mode (the flags' long names as keys: the
#                              listeners, the TLS files, `storage`, `state-store`,
#                              `api-token-file`); the default command names it, and the process
#                              exits 2 when it is missing. Not writable by group or others.
#   the files kbf.toml names   certificates, keys and the operator token, owned by uid 10001,
#                              a key or token with no group or other permission bit
#   /var/lib/kbf-server        a volume the frontend may write (a mounted volume replaces the
#                              ownership set below: make it uid 10001, mode 0750)
#
# The default listeners of `kbf` are on loopback, which no other container reaches: kbf.toml
# sets `listen`, `worker-listen` and `api-listen` to the addresses the orchestrator publishes.
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac
ARG COMMIT
# TLS to the state store, the storage nodes and the object store uses the system roots.
RUN DEBIAN_FRONTEND=noninteractive apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin kbf-server \
    && install -d -o 10001 -m 0750 /var/lib/kbf-server
COPY kbf /usr/bin/kbf
LABEL org.opencontainers.image.source="https://github.com/komira-ai/kbf" \
      org.opencontainers.image.revision="${COMMIT}"
USER 10001
# REAPI, the worker listener and the operator API, at the ports of docs/deploy/kbf-modes.md.
EXPOSE 8980 8981 8982
ENTRYPOINT ["/usr/bin/kbf"]
CMD ["--mode=frontend", "--config=/etc/kbf/kbf.toml"]
