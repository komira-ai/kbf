# The image of the update coordinator (docs/design/updates.md): the `kbf-coordinator` binary and
# nothing else. No installer, no updater and no CA key: the coordinator holds only its own
# certificate and key, the farm CA certificate it checks updaters against, and the desired-state
# file it writes.
#
# The build context holds the binary, built for the image's platform:
#
#   cargo build --release --locked -p kbf-coordinator
#   cp target/release/kbf-coordinator packaging/container/kbf-coordinator/
#   docker build packaging/container/kbf-coordinator
#
# Nothing builds or publishes this image yet: the release workflow does not package the
# coordinator (packaging/README.md). The base is pinned by its image index digest, as the
# images of the package install tests are (packaging/linux/test).
#
# Files the orchestrator mounts (the default flags below name them; every flag may be replaced):
#   /etc/kbf-coordinator/tls.pem, tls.key   the coordinator's certificate (role `coordinator`)
#   /etc/kbf-coordinator/ca.pem             the farm CA certificate
#   /etc/kbf-coordinator/oidc.toml          the allow-list of CI's OIDC token
#   /etc/kbf-coordinator/operator.token     the operator token, owned by uid 10001, mode 0600 or
#                                           0400 (the coordinator refuses any other mode)
#   /var/lib/kbf-coordinator                a volume the coordinator may write: the desired state
#                                           (a mounted volume replaces the ownership set below:
#                                           make it uid 10001, mode 0750)
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac
ENV DEBIAN_FRONTEND=noninteractive
# CI's OIDC keys are fetched over https with the system roots.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin kbf-coordinator \
    && install -d -o 10001 -m 0750 /var/lib/kbf-coordinator
COPY kbf-coordinator /usr/bin/kbf-coordinator
USER 10001
EXPOSE 8443 8444
ENTRYPOINT ["/usr/bin/kbf-coordinator"]
CMD ["--heartbeat-listen=0.0.0.0:8443", "--control-listen=0.0.0.0:8444", \
     "--tls-cert=/etc/kbf-coordinator/tls.pem", "--tls-key=/etc/kbf-coordinator/tls.key", \
     "--client-ca=/etc/kbf-coordinator/ca.pem", "--oidc-config=/etc/kbf-coordinator/oidc.toml", \
     "--operator-token-file=/etc/kbf-coordinator/operator.token", \
     "--state-file=/var/lib/kbf-coordinator/desired.json"]
