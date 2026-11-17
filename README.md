# kbf: the komira build farm

kbf is an open-source remote build and test farm for [Bazel](https://bazel.build)
and [Buck2](https://buck2.build). It speaks the
[Remote Execution API v2](https://github.com/bazelbuild/remote-apis) (REAPI v2):
a build tool sends actions to kbf, kbf runs them on worker machines, and the
results come back through a shared content-addressable cache.

kbf is written in Rust. Its programs are named `kbf-*`:

- `kbf-server`: the REAPI v2 service. It serves the action cache and the
  content-addressable storage, and schedules actions onto workers.
- `kbf-task`: runs one action's command and stages its outputs. It is the entrypoint of
  the job a js-agent runs for a placed action; a Linux node and a Mac both run actions
  only as js-agent jobs of the jobscheduler job service
  (`docs/design/actions-as-jobs.md`).

`kbf` is the binary a host runs, with the modes `frontend` and `storage`,
chosen with `kbf --mode=<mode>` (or `mode = "..."` in `/etc/kbf/kbf.toml`);
the programs above remain.
Smoke test: `kbf --mode=storage --help`

## Status

Early development. The project is design-first: interfaces and tests land
before features, and nothing here is ready for production use yet. Expect
breaking changes.

## Design

[ARCHITECTURE.md](ARCHITECTURE.md) describes how kbf is put together, and links the
design documents in [docs/design](docs/design): direct job submission, storage, the
execution path, and capabilities.

[docs/artifacts.md](docs/artifacts.md) says which binaries CI builds and attests, and
how a node verifies one before it runs it.

[docs/deploy/storage-node.md](docs/deploy/storage-node.md) sets up a storage host:
the server's unit, the priorities between MinIO and the server, and the memory
left to the host.
[docs/deploy/kbf-ui.md](docs/deploy/kbf-ui.md) is the operator guide to the read-only,
open web UI: how to reach it, how to turn it off, and what it exposes.
[docs/deploy/frontends-behind-a-load-balancer.md](docs/deploy/frontends-behind-a-load-balancer.md)
is the recommended deployment: several frontends behind a load balancer, bare-metal nodes,
one object store, and a live update that drains one frontend at a time (Envoy and compose
examples in [examples/deploy](examples/deploy)).
[docs/deploy/containers.md](docs/deploy/containers.md) runs the frontend and the update
coordinator as container images: the attested archives, their import without a registry, and
the files each one is given.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Every commit needs a DCO sign-off, and
every change meets the rules in [AGENTS.md](AGENTS.md).

## Security

To report a vulnerability, see [SECURITY.md](SECURITY.md). Do not put details of
a security problem in a public issue.

## License

kbf is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
