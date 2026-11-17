# Platform properties

A build tool describes where an action may run with REAPI platform properties
(`exec_properties` in Bazel and Buck2). kbf reads them from the `Action`'s platform,
or from the `Command`'s when the `Action` carries none. A property may appear once;
a repeated property is refused with `INVALID_ARGUMENT`.

These properties change how kbf schedules an action today:

| Property | Values | Default | Effect |
|---|---|---|---|
| `kbf-lease` | `action`, `whole_machine` | `action` | The kind of lease the action runs under (see [`kbf-lease`](#kbf-lease)). |
| `gpu` | a whole number | `0` | Whole GPUs the action needs. |
| `kbf-book-cpus` | a whole number, at least 1, plain digits | `1` | Whole cores the lease books. |
| `kbf-book-mem-gib` | a whole number, at least 1, plain digits | `4` (the first rung of `--memory-ladder-gib`) | GiB of memory the lease asks for: the limit it runs under, checked against the node's memory but not reserved (see [below](#kbf-book-cpus-and-kbf-book-mem-gib)). |
| `kbf-network` | `allow` | absent: no network | Lets the action's Command use the network (see [`kbf-network`](#kbf-network)). |
| `OSFamily` | `linux`; `darwin`, `macos`, `macosx`, `osx` (any case) | any | The operating system of the worker. |
| `ISA`, `Arch` | `x86-64`, `x86_64`, `amd64`; `arm-a64`, `arm64`, `aarch64`; an ISA level such as `x86-64-v3` (any case) | any | The CPU architecture, and level, of the worker. |

Any other value of `kbf-lease`, `gpu`, `kbf-book-cpus` or `kbf-book-mem-gib` is refused
with `INVALID_ARGUMENT`. kbf's own
capability keys (`os`, `arch`, `isa_level`, `cpu.feature`, ...; see
[capabilities.md](design/capabilities.md#matching)) are matched too. Other properties
(`container-image`, `Pool`, ...) are accepted and not acted on by the scheduler.

### Property names are read in any case

kbf reads every property name it knows without regard to ASCII case, so a property
meant for kbf is never ignored because of how it is spelled: `osfamily=darwin`,
`OSFAMILY=Darwin` and `OSFamily=darwin` all ask for a Mac. This holds for the REAPI
names (`OSFamily`, `ISA`, `Arch`) and for every kbf key: the capability keys above
(`OS` is `os`, `ISA_Level` is `isa_level`), `gpu` (`GPU=1` books a GPU), and the
reserved `kbf-lease`, `kbf-cpu`, `kbf-mac-admin`, `kbf-book-cpus` and
`kbf-book-mem-gib`. A `label.<k>` is not one of them in any case: node labels were
removed, so it is a name kbf does not read and is ignored like `Pool`.
`arch` spelled exactly so is kbf's own `arch` key (values `x86_64`, `arm64`); in any
other spelling (`Arch`, `ARCH`) it is REAPI's `Arch` and takes the values in the
table. Values are compared as described for each key: `OSFamily`, `ISA` and `Arch`
values in any case, kbf's own keys' values exactly.

One property sent under two spellings of its name (`gpu` and `GPU`, `OSFamily` and
`osfamily`) is refused with `INVALID_ARGUMENT`, as is one requirement named twice
through two names (`OSFamily` and `os`, `ISA` and `arch`). A name kbf does not know,
in any case, is not acted on.

`container-image` is the one exception: it is read by the executor (`--executor js-agent`),
not by the scheduler, and only in exactly that spelling. It names the base environment the
Command sees; an action that names an image other than the executor's base image is refused as
invalid, and one that names none gets the base ([kbf-modes.md](deploy/kbf-modes.md)).
`Container-Image` is a name kbf does not know, so the action is scheduled as if it named no
image.

## Where an action runs

An action runs only on a worker whose node report satisfies its platform: a Linux
action is never handed to a Mac, nor an arm64 action to an x86-64 machine. Buck2 and
Bazel name the platform with `OSFamily` (and `ISA` where the architecture matters):

```starlark
# Buck2: an execution platform's executor config
remote_execution_properties = {"OSFamily": "linux", "ISA": "x86-64"}
```

```starlark
# Bazel: a platform for actions that must run on the Macs
platform(
    name = "macos_arm64",
    exec_properties = {"OSFamily": "Darwin", "ISA": "arm-a64"},
)
```

An action no worker can ever run (an `OSFamily` other than Linux or macOS, an `ISA`
of another architecture) is refused at once with `FAILED_PRECONDITION`. An action that
no connected worker satisfies now waits: its operation stays `QUEUED`, and its metadata
carries a `google.rpc.ErrorInfo` (reason `NO_WORKER_CAN_RUN`, domain `kbf`) whose `why`
names the closest worker and what it lacks. The server logs it too. If no live worker
satisfies it for `--unservable-wait-secs` (300 by default; the wait restarts whenever
one does), it fails with `FAILED_PRECONDITION` and that reason. Work larger than every
worker that satisfies its platform (`gpu=2` where nodes have one) is treated the same.

An action that names no OS, no architecture and no ISA level (an empty platform, or one
with only properties kbf does not read, such as `pool`) is placed by
`--default-platform` (`linux/x86_64` by default), so it is never handed to a Mac that
merely sorts first. The default is not applied to an action that names `OSFamily` or
`ISA` alone, or asks for `xcode`, an iOS device or a VM image.
`--default-platform none` applies none. It changes placement only: the action digest and
the cache key are the client's. Each application is logged with the action digest, at
most once in ten seconds.

## `gpu`

`gpu=N` places the action only on a worker with `N` free GPUs, and books those GPUs
for the action's lease alone: no other lease is given them until that lease ends
(its result is accepted, or the lease is given up and the action requeued). A worker
with one GPU runs one `gpu=1` action at a time; work that asks for no GPU still
shares the worker's CPUs and memory beside it.

A worker reports how many GPUs it has in the `gpu` entry of its node report; a report
without one has none. The virtual workers of the executor report none (a job books no GPU),
so a `gpu=N` action waits and then fails as described under
[Where an action runs](#where-an-action-runs). A macOS action runs as a js-agent job
([actions-as-jobs.md](design/actions-as-jobs.md) section 14) and books no GPU slot.

In Bazel:

```starlark
cc_test(
    name = "kernel_test",
    exec_properties = {"gpu": "1"},
    ...
)
```

## `kbf-lease`

An `action` lease (the default) books cores and GPUs of a worker and asks for memory: one core and 4 GiB (the first rung of
the memory ladder), or what
`kbf-book-cpus` and `kbf-book-mem-gib` name, plus its `gpu` GPUs. A `whole_machine`
lease books every core, byte of memory and GPU of its worker, and runs there alone.

A lease goes only to a worker whose node report lists a driver for its kind: `container` for
`action`, `native-whole-machine` for `whole_machine` (see
[capabilities.md](design/capabilities.md#driver-entries)). The virtual workers of a Linux
executor list both; on a farm without such a worker an action waits with the reason and then
fails, as described under [Where an action runs](#where-an-action-runs). A macOS action runs
as a js-agent job ([actions-as-jobs.md](design/actions-as-jobs.md) section 14).

A `whole_machine` lease waits for a worker that holds no lease. While it waits it holds
one worker that could run it, and work queued after it at the same or a lower QoS is
not placed there, so that worker empties as its leases end. More urgent work is still
placed there. Running leases are never stopped for it.

## `kbf-book-cpus` and `kbf-book-mem-gib`

Every action asks for one core and 4 GiB of memory on the worker it runs on (the first
rung of the server's memory ladder, `--memory-ladder-gib`), unless it
names a size: `kbf-book-cpus=N` books `N` whole cores, and `kbf-book-mem-gib=N` books
`N` GiB. `N` is written in plain digits with no leading zero: `+4` and `04` are
refused, since each would book the same as `4` under a different action digest, and so
miss the cache entries `4` made. Either may be given alone; the other stays at its
default. Cores (and `gpu` GPUs) are booked: the scheduler places the action only on a
worker with that many free, and holds them for the lease until the lease ends. Memory is
not booked the same way: the worker's memory capacity must be at least the action's
memory, but what other leases on the worker hold of its memory never keeps the action
out. The worker bounds how many actions run at once by their cores, and its
memory-pressure backstop pauses new starts. An action that asks for the whole host's
memory (the last rung of the ladder) runs alone on its worker. The memory is the limit the
action runs under, below.

With the server's `--exclusive-actions` on, every `action` lease books the whole worker
it lands on instead: all of the worker's cores and all of its memory, as the worker reported
them when it registered, whichever worker that is. Exactly one action then runs per
worker, and a second waits for it, even on a worker of many cores. The setting is the
server's, not the action's, so it does not touch the action digest and the action cache is
shared with a server that has it off. `kbf-book-cpus` and `kbf-book-mem-gib` are still
checked (a malformed value is still refused) and still recorded as the client's hints, but
they no longer change what is booked, and `kbf-lease=whole_machine` and `vm` leases are as
they were. The lease's memory limit and CPU weight come from the booking, so the action may
use the worker's whole memory. An action killed for passing its own memory limit has no
larger booking to climb to: it finishes `FAILED_PRECONDITION` (out of memory) at once, as
one that already books the largest node does, and its key's memory floor is not raised.
The setting is not kept in the state store, so a server restart ends it for the operations
the store holds. An operation that was queued is read back as an ordinary `action`. One that
was running is recovered holding only the booking the front computed for it (by default one
core and the first memory rung), not the worker, so the scheduler may place ordinary
actions beside it; and if that lease is later lost or requeued, the operation runs again as
an ordinary `action`, climbing the memory ladder, with no claim on a worker. A run that
needs exactly one action per worker (a benchmark) must not restart the server, and
a restart during one invalidates it. The setting works on both scheduler cores.

The booking also sets the memory the action may use. The executor submits the job with the
memory the farm booked as a hard cap of 1.5 times the booking plus 512 MiB (6.5 GiB for the
default booking, 12.5 GiB for `kbf-book-mem-gib=8`), so a large `swiftc` or `ld` step that
dies at 2 GiB needs a larger booking.

The booking is where an action starts, not always where it runs. When a job ends because the
action passed its own memory limit, the server runs it again one rung up a
fixed ladder, 4, then 16, then 32 GiB, then a whole host (the largest node that could
run it, so that no other lease shares it; the rungs are the server's
`--memory-ladder-gib`). A size named here climbs from where it is: a killed 6 GiB ask
runs again with 16 GiB. The server remembers the raised booking for the action (its instance name and action digest), so
the next run of the same action starts there; only a kill at the largest node reaches
the client, as `FAILED_PRECONDITION` saying the action needs more memory than any node
offers. A job the host kills under the host's own memory pressure is rerun with the
same booking. See [scheduler.md](design/scheduler.md#memory-kills).

```starlark
# Bazel: a link step that needs 4 cores and 12 GiB
cc_binary(
    name = "server",
    exec_properties = {"kbf-book-cpus": "4", "kbf-book-mem-gib": "12"},
    ...
)
```

A value that is not a whole number of at least 1 is refused with `INVALID_ARGUMENT`,
as is either key on a `kbf-lease=whole_machine` lease, which books the whole
worker. A booking larger than any worker that satisfies the platform waits and
then fails, as described under [Where an action runs](#where-an-action-runs).

Like every platform property, the two keys are part of the action digest: the same
action with another booking is a different action cache entry, so changing a booking
reruns the action once. They are not the capability keys `cpus` and `mem_gib`, which
ask for a worker whose whole machine has at least that much and book nothing.

## macOS, Xcode, simulators, VMs and iOS devices

A macOS action runs as a js-agent job
([actions-as-jobs.md](design/actions-as-jobs.md) section 14). No worker reports the `xcode`,
`vm.image`, `vm.slots`, `ios.device` or `gpu` entries of a node report, and there is no
`simulator` property. The server still reads those report keys (the wire fields stay), so an
action that names `xcode`, `vm.image` or `ios.device` matches no worker: it waits and then fails
as described under [Where an action runs](#where-an-action-runs). A `vm.image` value without a
digest, a `vm.slots`, `vm.max_cpus` or `vm.max_mem_gib` key, a value of `ios.device` other than
`1`, an `ios.device.<attribute>` without `ios.device=1`, and `kbf-lease=vm` are refused with
`INVALID_ARGUMENT`.

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
