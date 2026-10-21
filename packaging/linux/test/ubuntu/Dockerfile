# The fresh system the deb install test (cargo xtask pkg-install-test --format deb)
# installs into: Ubuntu 24.04 with systemd as PID 1, pinned by its image index digest.
# Nothing of the package's dependencies is installed: apt resolves them from the .deb.
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends systemd systemd-sysv
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
