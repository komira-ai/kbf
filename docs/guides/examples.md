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
  front, the front ends TLS for the client-facing name. Who may call is set by
  `--reapi-auth-policy` ([reapi-auth.md](../reapi-auth.md)), not by a client certificate.
  The policy variants that read a client certificate or a token are planned, not built.
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
build:kbf --remote_executor=grpcs://frontend.example.com
build:kbf --remote_cache=grpcs://frontend.example.com
build:kbf --remote_instance_name=main
build:kbf --remote_timeout=300s
build:kbf --remote_default_exec_properties=OSFamily=Linux
build:kbf --remote_default_exec_properties=ISA=x86-64
build:kbf --remote_default_exec_properties=container-image=docker://registry.example.com/build@sha256:<digest>
```

Use it with `bazel build --config=kbf //...`.

- `--remote_executor` takes `grpc`, `grpcs` or `unix`, and `grpcs` is the default when no
  scheme is given. `--remote_cache` takes the same kinds of address.
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
  not authenticate you to kbf. A front in front of kbf may use them.
- `--remote_header=Name=Value` adds a header to every request, for a front that wants one.

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
    name = "server",
    exec_properties = {"kbf-book-cpus": "4", "kbf-book-mem-gib": "12"},
    ...
)
```

Pass the platforms with Bazel's own platform flags (`--platforms`,
`--extra_execution_platforms`); those are Bazel's, not kbf's.

## Buck2

Put the farm's addresses in the `[buck2_re_client]` section of `.buckconfig`:

```text
# .buckconfig
[buck2]
  digest_algorithms = SHA256

[buck2_re_client]
  engine_address = grpc://frontend.example.com:443
  action_cache_address = grpc://frontend.example.com:443
  cas_address = grpc://frontend.example.com:443
  tls = true
  tls_ca_certs = /path/to/ca.pem
  instance_name = main
```

- `engine_address`, `action_cache_address` and `cas_address` are the engine, the action
  cache and the CAS. For kbf all three are the REAPI address.
- Write the scheme as `grpc://` even when TLS is on, and turn TLS on with `tls = true`.
  Buck2 refuses a `grpcs://` address.
- `tls_ca_certs` is a PEM CA bundle to trust. Without it Buck2 uses a default bundle,
  which is right for a front with a public certificate.
- `tls_client_cert` is Buck2's setting for a client certificate and key. kbf's REAPI
  listener does not ask for one.
- `instance_name` is passed on execution, action cache and CAS requests. Leave it out for
  the empty instance name.
- `http_headers` (a comma-separated list of `Header: Value`) is sent with every request,
  for a front that wants one.
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
action goes to a Linux worker and a Darwin action to a Mac.

## Platform properties kbf reads

[platform-properties.md](../platform-properties.md) is the full list. The ones these
examples use:

| Property | Value | Effect |
|---|---|---|
| `OSFamily` | `linux`, or `darwin`, `macos`, `macosx`, `osx` | The operating system of the worker. |
| `ISA`, `Arch` | `x86-64`, `x86_64`, `amd64`; or `arm-a64`, `arm64`, `aarch64`; or an ISA level such as `x86-64-v3` | The CPU architecture of the worker. |
| `container-image` | `docker://<repo>@sha256:<digest>` | The image a Linux action runs in. |
| `kbf-book-cpus` | whole number, 1 or more | Cores the action books. |
| `kbf-book-mem-gib` | whole number, 1 or more | GiB of memory the action books. |

Rules for the names and values:

- Names and the values of `OSFamily`, `ISA` and `Arch` are read in any case, so
  `osfamily=darwin`, `OSFamily=Darwin` and `OSFAMILY=DARWIN` all ask for a Mac. `Arch` is
  accepted as `ISA`. A name sent twice, in any two spellings, is refused with
  `INVALID_ARGUMENT`.
- `container-image` is the exception: it is read only in exactly that spelling.
  `Container-Image` is ignored, and the container driver then refuses the action.
- An action that names no OS, no architecture and no ISA level goes where
  `--default-platform` says, `linux/x86_64` by default. Name `OSFamily` and `ISA` so a
  Mac and a Linux box are never confused.
- An action that no connected worker can run waits, `QUEUED`, with the reason
  `NO_WORKER_CAN_RUN` in its metadata, and fails with `FAILED_PRECONDITION` after
  `--unservable-wait-secs` (300 by default).

### The container image

A Linux container action names its image by digest:

```text
container-image=docker://registry.example.com/build@sha256:<digest>
```

- Use the digest of **one architecture's manifest**, not the digest of the multi-architecture
  image index. kbf refuses an index with "name the per-architecture manifest digest".
- A tag is refused: an image is named only by digest, so one action digest never means two
  sets of bytes.
- The digest is `sha256:` and 64 lowercase hex digits.
- A node fetches the image from the registry by that digest, checks every byte against it,
  and unpacks it once. A daemon started with `--image docker://...@sha256:<digest>` has the
  image ready before the first action.
- Only the container driver, which Linux workers use, reads `container-image`. Mac workers
  run actions as plain processes and ignore it.

### Memory booking

Every action books 1 core and a share of memory on the worker it runs on. Two properties
change the booking:

- `kbf-book-cpus=N` books `N` whole cores.
- `kbf-book-mem-gib=N` books `N` GiB of memory.

Write `N` in plain digits with no sign or leading zero: `+4` and `04` are refused,
because each would book the same as `4` under a different action digest. Either key may be
given alone. The scheduler places the action only where that much is free.

The booking is also the action's memory limit. The native driver on a Mac kills an action
whose processes together hold more than 150% of the booked memory plus 512 MiB: 12.5 GiB
for `kbf-book-mem-gib=8`. The container driver sets a hard cap from the booking.

When a worker reports that an action ran past its own memory limit, the server runs it
again one step up the memory ladder (4, 16, 32 GiB, then a whole host) and remembers the
raised booking for that action, so the next run starts there. A large `swiftc` or `ld`
step that dies at the default needs a larger `kbf-book-mem-gib`. Both keys are part of the
action digest, so changing a booking reruns the action once.

Neither key is allowed on a `kbf-lease=whole_machine` action, which books the whole worker.

## All-in-one on one machine

`kbf all-in-one` runs the REAPI service, the scheduler, the storage and a daemon in one
process. It is for a trusted single box or for development: builds run beside the farm's
state and its cold store, as one user. It uses the container driver only.

This is the flag set kbf's unit template uses, with the certificate files you need as
placeholders:

```text
kbf all-in-one \
    --state-store=sqlite:///var/lib/kbf/state.db \
    --cold-store=file:///var/lib/kbf/cold \
    --meta-dir=/var/lib/kbf/meta \
    --listen=127.0.0.1:8980 \
    --worker-listen=127.0.0.1:8981 \
    --worker-tls-cert=/etc/kbf/worker.pem \
    --worker-tls-key=/etc/kbf/worker.key \
    --worker-client-ca=/etc/kbf/ca.pem \
    --api-listen=127.0.0.1:8982 \
    --api-token-file=/etc/kbf/api-token \
    --driver=container \
    --oci-runtime=/usr/bin/crun \
    --server=https://localhost:8981 \
    --ca-cert=/etc/kbf/ca.pem \
    --cert=/etc/kbf/node.pem \
    --key=/etc/kbf/node.key \
    --node-id=node-1 \
    --data-dir=/var/lib/kbf
```

- `--listen` is the REAPI address your clients use. It serves plain text here. Without
  `--reapi-tls-cert` and `--reapi-tls-key` it serves plain text on any address, and off
  loopback the start line warns.
- `--worker-listen` is where the daemon half connects, over mutual TLS with the three
  `--worker-*` certificate flags. `--server`, `--ca-cert`, `--cert` and `--key` are the
  daemon half's side of that connection.
- `--cold-store=file://...` is a directory, so the box needs no S3 service and holds no key.
  Add `--hot-store=file:///fast/kbf` to put a fast disk in front of it.
- `--state-store` names the scheduler's state. The default of `kbf all-in-one` is Postgres
  on the host's socket; this example names a SQLite file instead.
- `--api-token-file` must be a file of mode 0600 or 0400 holding one token of at least 32
  characters. The operator API refuses writes without it.
- The daemon runs rootless containers with `crun`. It needs the user's subordinate ids,
  `newuidmap` and `newgidmap` on `PATH`, and a delegated cgroup; its unit sets
  `Delegate=yes`. [kbf-modes.md](../deploy/kbf-modes.md) has the unit and its checks.
- `kbf --mode=all-in-one --help` lists every flag.

The client side, on the same machine:

```text
# .bazelrc
build:kbf --remote_executor=grpc://127.0.0.1:8980
build:kbf --remote_cache=grpc://127.0.0.1:8980
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

An all-in-one farm on an arm64 machine should start with `--default-platform=none`, or its
actions must name `ISA=arm64`; the default places an action that names no platform on
`linux/x86_64`.
