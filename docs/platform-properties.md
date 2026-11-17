# Platform properties

A build tool describes where an action may run with REAPI platform properties
(`exec_properties` in Bazel and Buck2). kbf reads them from the `Action`'s platform,
or from the `Command`'s when the `Action` carries none. A property may appear once;
a repeated property is refused with `INVALID_ARGUMENT`.

kbf no longer schedules ([direct-js.md](design/direct-js.md)): every Execute that misses the
action cache is one job of the job service, which decides where and when it runs. These properties
are the ones kbf reads when it makes that job:

| Property | Values | Default | Effect |
|---|---|---|---|
| `kbf-lease` | `action`, `whole_machine` | `action` | `whole_machine` makes the job ask for a whole host, with nothing beside it (see [`kbf-lease`](#kbf-lease)). |
| `OSFamily`, `os` | `linux`; `darwin`, `macos`, `macosx`, `osx` (any case) | `linux` | A macOS value makes the job a Mac job (see [macOS](#macos)). |
| `os_version` | a version such as `26` or `26.1` | any | The least macOS a Mac action takes; read only for a Mac action. |
| `container-image` | the executor's base image, by digest | the base image | The base environment the Command sees. |
| `kbf-network` | `allow` | absent: no network | Lets the action's Command use the network (see [`kbf-network`](#kbf-network)). |

Any other value of `kbf-lease` is refused with `INVALID_ARGUMENT`. Every other property
(`gpu`, `ISA`, `Arch`, `Pool`, ...) is accepted and does nothing: kbf does not match an action
to a worker, books no cores, GPUs or memory for it, and reads neither `kbf-book-cpus` nor
`kbf-book-mem-gib` (the job asks for no CPU and no memory; the job service sizes it from the
history of the action's build target, which kbf gives it as the job's `size-class` label). Like
every platform property they are part of the action digest, so changing one reruns the action
once.

### Property names are read in any case

kbf reads every property name it knows without regard to ASCII case, so a property
meant for kbf is never ignored because of how it is spelled: `osfamily=darwin`,
`OSFAMILY=Darwin` and `OSFamily=darwin` all ask for a Mac. This holds for `OSFamily` and
for the kbf keys (`OS` is `os`, and the reserved `kbf-lease`). Values are compared as described
for each key: `OSFamily` values in any case, kbf's own keys' values exactly.

One property sent under two spellings of its name (`gpu` and `GPU`, `OSFamily` and
`osfamily`) is refused with `INVALID_ARGUMENT`, as is one requirement named twice
through two names (`OSFamily` and `os`, `ISA` and `arch`). A name kbf does not know,
in any case, is not acted on.

`container-image` is read by the executor (`--executor js-agent`), and only in exactly that
spelling. It names the base environment the Command sees; an action that names an image other
than the executor's base image is refused as invalid, and one that names none gets the base
([kbf-modes.md](deploy/kbf-modes.md)). `Container-Image` is a name kbf does not know, so the
action runs as if it named no image.

## Where an action runs

kbf does not place an action. The job it makes asks for a Linux host of the executor's
architecture (`--executor-arch`) in a container, or, for an action that names a Mac, a Mac of the
job service's. Buck2 and Bazel name the platform with `OSFamily`:

```starlark
# Buck2: an execution platform's executor config
remote_execution_properties = {"OSFamily": "linux"}
```

```starlark
# Bazel: a platform for actions that must run on the Macs
platform(
    name = "macos_arm64",
    exec_properties = {"OSFamily": "Darwin"},
)
```

An action that asks for something the executor cannot run (a Mac when no Macs are configured, a
whole machine on a Mac, a `container-image` other than the base) is refused at once as the
caller's, `INVALID_ARGUMENT`. kbf never waits for a worker that satisfies a platform: an action
the job service cannot place is the job service's to hold, and kbf reads its job back until it
ends.

## `kbf-lease`

`kbf-lease=action` (the default) is an ordinary job. `kbf-lease=whole_machine` makes the job
need a whole host of the executor's architecture and nothing beside it, for the action's timeout
and the run margin. It cannot be a put-directory job (`--task-out-mode put-dir`), and a Mac
action cannot ask for it: both are refused as the caller's. This is the only property kbf still
reads to size the job; the others about a lease, a booking or a GPU (`gpu`, `kbf-book-cpus`,
`kbf-book-mem-gib`) are not read.

A job that ends because it passed its memory limit is submitted again as the next attempt, at
most twice, and then answered `FAILED_PRECONDITION` (out of memory, reason `ACTION_OUT_OF_MEMORY`);
there is no ladder of sizes and kbf remembers no size for the action.

## macOS

An action whose platform names `OSFamily` (or `os`) `macos`, `darwin`, `macosx` or `osx` runs as a
js-agent job on a Mac of the job service ([actions-as-jobs.md](design/actions-as-jobs.md)
section 14), when the executor is given the release's macOS `kbf-task` bundle
(`--executor-mac-task-bundle`); without it the action is refused. `os_version=26` means macOS 26
or newer, held against `--executor-mac-os-version`. kbf reads no `xcode`, `vm.image`, `ios.device`
or simulator property: they are accepted and do nothing.

## `kbf-network`

The Command of an action runs without the network: `kbf-task`, the tool that runs it inside
the job, installs a seccomp filter in the Command's process before it executes, and every
process the Command starts inherits it. The filter refuses, with `EPERM`, `socket` and
`socketpair` for every domain but `AF_UNIX` (so `AF_INET`, `AF_INET6`, `AF_NETLINK`,
`AF_PACKET`, `AF_VSOCK` and the rest), and `io_uring_setup`, `io_uring_enter` and
`io_uring_register` (a ring can make a socket without calling `socket`). A call under another
system call ABI is killed with `SIGSYS` (exit code 159). It needs no capability, and it sets
`no_new_privs`, so a setuid program the Command starts gains nothing. Only `kbf-task`'s own
fetch of the inputs and upload of the pack use the network.

`kbf-network=allow` turns that off for one action, for a test that needs a real socket or a
step that fetches from the internet; `kbf-task` writes one line naming the action digest and
the property to its standard error. Any other value (`Allow`, `true`, an empty one) or the
property given twice is refused: the action fails without running and `kbf-task` exits 11
(run-start). The name is read in any case. Like every platform property it is part of the
action digest, so the same action with and without it are two cache entries.

Two layers keep the Command off the network. First, the Command gets a user and network
namespace of its own, with its own user and group id inside (`unshare` of
`CLONE_NEWUSER|CLONE_NEWNET`, a map of its own ids to themselves, `lo` raised): it sees one
interface, `lo`, and has no route anywhere else. Then the seccomp filter goes in. Which filter
depends on whether every namespace step succeeded:

| every namespace step succeeded | the filter lets the Command make |
|---|---|
| yes | `AF_UNIX`, `AF_INET` and `AF_INET6` sockets: `127.0.0.1` works (a test can start a server and connect to it); a connect to any other address fails with `ENETUNREACH`. `AF_NETLINK`, `AF_PACKET`, every other domain and io_uring are refused |
| no | `AF_UNIX` sockets only: every `AF_INET` and `AF_INET6` socket is refused with `EPERM`, as are all the others above |

The namespace needs the host to let an unprivileged user namespace keep its capabilities.
Where an AppArmor setting restricts that (Ubuntu's `apparmor_restrict_unprivileged_userns`,
which a container profile that allows `userns` lifts) a step fails with `EPERM`; the run goes
on under the strict filter, and a Command whose maps could not be written sees itself as the
overflow user (65534). `kbf-task` writes one line per action to standard error saying which
mode it ran in:

```
kbf-task: action <digest>: network: netns (loopback only) + seccomp
kbf-task: action <digest>: network: seccomp only, no inet (netns: <step> <errno>)
kbf-task: action <digest>: platform property kbf-network=allow: the Command runs with the network
```

where the step is `unshare`, `setgroups`, `uid_map`, `gid_map` or `loopback`. The same `unshare`
also makes the Command's mount and PID namespaces, whatever this property says, and a second line
says how they came out: `kbf-task: action <digest>: sandbox: mount and pid namespaces`, or
`sandbox: incomplete (<step> <errno>)` (what the Command sees in them is in
[kbf-modes.md](deploy/kbf-modes.md), "What an action's Command sees in its job"). A Command that
runs as root (uid 0) gets no user or network namespace, because a user namespace that maps only its own id would
make `chown` to another user, `setuid` and `setgroups` fail for it (`tar`, `dpkg`, `apt`); it
runs under the strict filter, and the line says `network: seccomp only, no inet (netns:
skipped for uid 0)`.
`kbf-task` also makes itself non-dumpable (`PR_SET_DUMPABLE` 0) before it opens a socket, so
the Command, which runs as the same user, cannot `ptrace` it, take its sockets with
`pidfd_getfd` or read its memory through `/proc`.
On macOS `kbf-task` has no seccomp filter or namespace. It runs the Command alone under
`/usr/bin/sandbox-exec`, with the profile of `crates/kbf-seatbelt`: everything is allowed except handing work to `launchd` and writing a
preference, a Command denied the network gets a denial of every network operation but loopback and Unix sockets, and for every Command reading the files this tool holds a
credential or presigned URL in is denied, as are writing it and renaming or removing the
directories above it (`--grant-token-file`, `--pack-url-file`, `--put-urls-file`). A Command with `kbf-network=allow` gets the profile without the network
clause. The line for a denied Command is `kbf-task: action <digest>: network: sandbox-exec profile
(no network but loopback and unix sockets)`. The limits are the profile's: any Unix socket stays
open, so DNS lookups still go out through the system's resolver, and a connect to the node's own
non-loopback address was allowed under the denial on the macOS runner. The profile does not limit
other writes, helper daemons reached by `mach-lookup` and other services on the node stay reachable, and a credential given as a flag value (`--insecure-argv-secrets`) stays in this tool's
argv. The macOS `kbf-task` is released only as the bundle a Mac job runs
([artifacts.md](artifacts.md#the-macos-kbf-task-bundle)), and runs the Command directly there: the
agent's sandbox is its sandbox, and this profile is not applied. On a system that is neither Linux nor macOS there is
no confine, and a Command denied the network is refused there (exit 15).
