# The fresh system the rpm install test (cargo xtask pkg-install-test --format rpm)
# installs into: Fedora 44 with systemd as PID 1, pinned by its image index digest.
# Nothing of the package's dependencies is installed: dnf resolves them from the .rpm.
FROM fedora:44@sha256:43b29f65a41eb9c35e1cd5323e3bdf3b655c2357a9f4f1ff2f9c2798e5045d80
RUN dnf install -y systemd && dnf clean all
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
