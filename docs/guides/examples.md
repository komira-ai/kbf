# Examples: pointing Bazel and Buck2 at kbf

kbf speaks the Remote Execution API v2 on one gRPC address, the REAPI listener
(`--listen`, port 8980 by default). The same address serves the action cache, the CAS and
execution, so a client names it for all three. This page shows the client settings for
Bazel and Buck2, the platform properties kbf reads, and a one-machine farm. The addresses
below are placeholders: `frontend.example.com` stands for your REAPI address and `<digest>`
for an image digest you look up.

What kbf supports on this listener, so the examples make sense:

- Digests are SHA-256 only.
- The listener serves plain text, or TLS with a server certificate (`--reapi-tls-cert`,
  `--reapi-tls-key`). It asks clients for **no client certificate**. Behind a TLS-terminating
  front, the front ends TLS for the client-facing name. Who may call the listener itself is
  set by `--reapi-auth-policy` ([reapi-auth.md](../reapi-auth.md)), not by a client
  certificate. The bundled proxy of a packaged frontend host can require a bearer token or a
  client certificate in front of the listener; see
  [Authentication behind the bundled proxy](#authentication-behind-the-bundled-proxy).
- Clients never write the action cache. `UpdateActionResult` is always refused with
  `PERMISSION_DENIED`, so a client's own cache uploads of results do not reach kbf's action
  cache. Only the farm stores results of actions it ran.
- An instance name is accepted and optional. It gates calls when a policy has an
  `instanceNamePrefix` authorizer, and identical actions share one run only within one
  instance name. It does not separate data: the CAS and the action cache ignore it.

## Bazel

A `.bazelrc` for a farm behind a TLS front:

```text
# .bazelrc
build:kbf --remote_executor=grpcs://frontend.example.com:8980
build:kbf --remote_cache=grpcs://frontend.example.com:8980
build:kbf --spawn_strategy=remote
build:kbf --noremote_local_fallback
build:kbf --noremote_upload_local_results
build:kbf --remote_instance_name=main
build:kbf --remote_timeout=300s
build:kbf --remote_default_exec_properties=OSFamily=Linux
build:kbf --remote_default_exec_properties=ISA=x86-64
build:kbf --remote_default_exec_properties=container-image=docker://registry.example.com/build@sha256:<digest>
```

Use it with `bazel build --config=kbf //...`.

- `--spawn_strategy=remote` and `--noremote_local_fallback` keep actions from running on
  your machine. `--noremote_upload_local_results` stops Bazel from uploading the results of
  actions that did run locally: only the farm writes the action cache, and kbf refuses
  `UpdateActionResult` (see above), so without the flag Bazel tries an upload for every
  local action and gets `PERMISSION_DENIED`.
- `--remote_executor` takes `grpc`, `grpcs` or `unix`, and `grpcs` is the default when no
  scheme is given. Without a port, `grpcs` means port 443, and kbf listens on 8980, so
  write the port. `--remote_cache` takes those addresses and also `http://` and `https://`.
- `--remote_instance_name` is passed as the instance name. Leave it out for the empty
  instance name.
- `--remote_default_exec_properties` gives the properties for an execution platform that
  sets none of its own. It can be repeated, one `key=value` each.
- For a farm on a plain-text address, such as one machine on loopback, use
  `grpc://127.0.0.1:8980`.
- For a farm whose TLS certificate comes from the farm's own CA, add
  `--tls_certificate=/path/to/ca.pem`, a certificate trusted to sign the server's
  certificate.
- Bazel also has `--tls_client_certificate` and `--tls_client_key` for client
  authentication. kbf's REAPI listener does not ask for a client certificate, so they do
  not authenticate you to the listener itself. The bundled proxy in front of it may require
  one (see below).
- `--remote_header=Name=Value` adds a header to every request, for a proxy that wants one.

Per-target or per-platform properties go in `exec_properties` on a `platform`:

```starlark
# BUILD
platform(
    name = "linux_x86_64",
    exec_properties = {
        "OSFamily": "Linux",
        "ISA": "x86-64",
        "container-image": "docker://registry.example.com/build@sha256:<digest>",
    },
)

platform(
    name = "macos_arm64",
    exec_properties = {"OSFamily": "Darwin", "ISA": "arm-a64"},
)

cc_binary(
    name = "link_everything",
    exec_properties = {"kbf-lease": "whole_machine"},
    ...
)
```

`exec_properties` on a `platform` take effect when that platform is an execution platform.
Make it one with `--extra_execution_platforms=//:linux_x86_64`, or with
`register_execution_platforms` in `MODULE.bazel`. `--platforms` names the target platform
and does not choose where an action runs. These flags are Bazel's, not kbf's. If you do not
want platform targets, `--remote_default_exec_properties` above is simpler.

The `container-image` value in the `.bazelrc` above applies to Linux actions only; an action
that runs on a Mac ignores it (see [The container image](#the-container-image)). The
`macos_arm64` platform therefore does not set it.

## Buck2

Put the farm's addresses in the `[buck2_re_client]` section of `.buckconfig`:

```text
# .buckconfig
[buck2]
  digest_algorithms = SHA256

[buck2_re_client]
  engine_address = grpc://frontend.example.com:8980
  action_cache_address = grpc://frontend.example.com:8980
  cas_address = grpc://frontend.example.com:8980
  tls = true
  tls_ca_certs = /path/to/ca.pem
  instance_name = main
  execution_concurrency_limit = 1000
```

These are the settings kbf's own Buck2 sample projects use.

- `engine_address`, `action_cache_address` and `cas_address` are the engine, the action
  cache and the CAS. For kbf all three are the REAPI address.
- Write the scheme as `grpc://` even when TLS is on, and turn TLS on with `tls = true`.
  Buck2 refuses a `grpcs://` address.
- `tls_ca_certs` is a PEM CA bundle to trust. Without it Buck2 uses a default bundle,
  which is right for a front with a public certificate.
- `tls_client_cert` is Buck2's setting for a client certificate and key. kbf's REAPI
  listener does not ask for one, but the bundled proxy may require one (see below).
- `execution_concurrency_limit` caps how many actions Buck2 runs remotely at once. Buck2
  keeps one `Execute` stream open per outstanding action, and the REAPI listener allows
  10000 streams (`--reapi-max-streams`), so keep the limit below that.
- `instance_name` is passed on execution, action cache and CAS requests. Leave it out for
  the empty instance name.
- `http_headers` (a comma-separated list of `Header: Value`) is sent with every request,
  for a proxy that wants one.
- For a plain-text address on a trusted network, use `tls = false` and
  `grpc://<host>:8980`.

The properties go in the execution platform's `CommandExecutorConfig`. This is a platform
rule for a Linux container image:

```starlark
def _remote_platform_impl(ctx):
    platform = ExecutionPlatformInfo(
        label = ctx.label.raw_target(),
        configuration = ConfigurationInfo(constraints = {}, values = {}),
        executor_config = CommandExecutorConfig(
            local_enabled = False,
            remote_enabled = True,
            use_limited_hybrid = False,
            remote_execution_properties = {
                "OSFamily": "linux",
                "ISA": "x86-64",
                "container-image": ctx.attrs.container_image,
            },
            remote_execution_use_case = "buck2-default",
            remote_output_paths = "output_paths",
        ),
    )
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = [platform])]

remote_platform = rule(
    impl = _remote_platform_impl,
    attrs = {"container_image": attrs.string()},
)
```

For a Mac, use the same rule with different properties:

```starlark
remote_execution_properties = {"OSFamily": "Darwin", "ISA": "arm-a64"},
```

Buck2 has one remote execution service per checkout, so every remote action of a checkout
goes to the one `engine_address`. kbf routes each action by its `OSFamily`, so a Linux
action goes to a Linux host and a Darwin action to a Mac.

## Authentication behind the bundled proxy

A packaged frontend host runs a proxy in front of the REAPI listener. The proxy's
`buck2_auth` setting decides who may call; the setup flags are in
[kbf-installer-setup.md](../deploy/kbf-installer-setup.md). There are three modes.

- `none`: anyone who can reach the address may call. Use it only on a network you trust.
- `bearer`: every call must carry `Authorization: Bearer <token>`. A client certificate is
  neither needed nor enough, and a wrong token is refused even next to a good certificate.
  In Bazel, add `--remote_header=Authorization=Bearer <token>`. In Buck2, set
  `http_headers = Authorization: Bearer <token>` in `[buck2_re_client]`.
- `mtls`: every call must present a client certificate that the proxy verifies. In Bazel,
  set `--tls_client_certificate` and `--tls_client_key`. In Buck2, set `tls_client_cert`.
  These do authenticate you to the proxy.

A token on the command line or in a checked-in file is readable by others: keep it in a
user-local `.bazelrc` or `.buckconfig.local` that you do not commit.

## Platform properties kbf reads

[platform-properties.md](../platform-properties.md) is the full list. The ones these
examples use:

| Property | Value | Effect |
|---|---|---|
| `OSFamily` | `linux`, or `darwin`, `macos`, `macosx`, `osx` | `linux` (the default) or a Mac job. |
| `container-image` | `docker://<repo>@sha256:<digest>` | The base image a Linux action's Command sees; it must be the frontend's base image. |

Rules for the names and values:

- The name and the value of `OSFamily` are read in any case, so
  `osfamily=darwin`, `OSFamily=Darwin` and `OSFAMILY=DARWIN` all ask for a Mac. A name sent
  twice, in any two spellings, is refused with `INVALID_ARGUMENT`.
- `container-image` is the exception: it is read only in exactly that spelling.
  `Container-Image` is ignored, and the action gets the frontend's base image.
- kbf does not match an action to a worker: `ISA`, `Arch`, `gpu`, `kbf-book-cpus` and
  `kbf-book-mem-gib` are accepted and do nothing. The job asks for no CPU and no memory, and the job
  service sizes it from the history of the action's build target.

### The container image

A Linux action may name the base environment its Command sees, by digest:

```text
container-image=docker://registry.example.com/build@sha256:<digest>
```

- The frontend is started with one base image, `--executor-base-image`, pinned by digest. An
  action that names another image is refused as invalid, and one that names none gets the base
  image.
- A tag is refused: an image is named only by digest, so one action digest never means two
  sets of bytes.
- The digest is `sha256:` and 64 lowercase hex digits.
- An action that runs on a Mac ignores `container-image`: a Mac's agent runs the release's
  `kbf-task` bundle.

### Memory

An action books no memory: the job carries no CPU or memory, and the job service sizes it from the
history of the action's build target (its `size-class` label), or, while the target has run too
few times, from that of its kind (what runs, its `size-fallback` label). When the job service reports that an
action ran past its memory limit, the frontend submits it again as the next attempt, at most twice,
and then answers `FAILED_PRECONDITION` (out of memory). `kbf-lease=whole_machine` makes the job ask
for a whole host instead.

## A frontend and a storage node on one machine

A small farm is two processes of `kbf`, a `frontend` and a `storage` node, with the job service
on its own. They can share a machine for a trusted single box or for development; the storage
node holds the blobs and the metadata log, the frontend serves REAPI, schedules and places each
action as a job of the job service (`--executor js-agent`). `kbf --mode=frontend --help` and
`kbf --mode=storage --help` list every flag; the modes are described in
[kbf-modes.md](../deploy/kbf-modes.md), and a one-host layout in
[single-host.md](../deploy/single-host.md).

The storage node (the certificates a node needs on a farm are listed in
[kbf-units.md](../deploy/kbf-units.md)):

```text
kbf --mode=storage \
    --cold-store=file:///var/lib/kbf/cold \
    --meta-dir=/var/lib/kbf/meta \
    --storage-listen=127.0.0.1:8982
```

The frontend:

```text
kbf --mode=frontend \
    --listen=127.0.0.1:8980 \
    --storage=http://127.0.0.1:8982 \
    --state-store=postgres:///kbf?host=/run/postgresql \
    --api-listen=127.0.0.1:8983 \
    --api-token-file=/etc/kbf/api-token \
    --executor=js-agent
```

The executor needs more flags (`--executor-job-url`, `--executor-job-token-file`, the base image
and base tree); they are listed in [kbf-modes.md](../deploy/kbf-modes.md) and
[job-service-cutover.md](../deploy/job-service-cutover.md).

- `--listen` is the REAPI address your clients use. Without `--reapi-tls-cert` and
  `--reapi-tls-key` it serves plain text on any address, and off loopback the start line warns.
- Without the TLS flags the storage node serves plain text and its address must be loopback;
  off one machine, give both processes the certificate flags.
- `--cold-store=file://...` is a directory, so the box needs no S3 service and holds no key.
  Add `--hot-store=file:///fast/kbf` to the storage node to put a fast disk in front of it.
- `--state-store` names where the executor keeps its open operations and finished answers: a Postgres URL. A `sqlite://` store still starts in
  this release and logs a warning that it is removed in the next.
- `--api-token-file` must be a file of mode 0600 or 0400 holding one token of at least 32
  characters. The operator API refuses writes without it.

The client side, on the same machine:

```text
# .bazelrc
build:kbf --remote_executor=grpc://127.0.0.1:8980
build:kbf --remote_cache=grpc://127.0.0.1:8980
build:kbf --spawn_strategy=remote
build:kbf --noremote_local_fallback
build:kbf --noremote_upload_local_results
build:kbf --remote_default_exec_properties=OSFamily=Linux
build:kbf --remote_default_exec_properties=container-image=docker://registry.example.com/build@sha256:<digest>
```

```text
# .buckconfig
[buck2_re_client]
  engine_address = grpc://127.0.0.1:8980
  action_cache_address = grpc://127.0.0.1:8980
  cas_address = grpc://127.0.0.1:8980
  tls = false
```

A farm whose job hosts are arm64 machines starts the frontend with `--executor-arch=arm64`.

Planned: client-certificate and token variants of `--reapi-auth-policy` on the REAPI
listener itself; today the bundled proxy supplies bearer and mTLS authentication in front
of it.
