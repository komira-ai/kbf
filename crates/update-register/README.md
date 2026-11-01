# update-register

The client of the registration socket of a host updater
([docs/design/host-updates.md](../../docs/design/host-updates.md), 2.1). A service links it,
registers when it starts and holds the registration for as long as it runs.

```rust
let _registration = update_register::register(
    std::path::Path::new("/run/updater/register.sock"),
    "tierstore",
    "1.4.0",
    Some(&update_register::Health::new("127.0.0.1:9301".parse()?, "/readyz")),
)?;
```

- The message is one JSON line: the service name, the version it runs and, if the service has a
  listener, a loopback address and path that answer 2xx when it is ready (`health`). A service
  with no listener passes `None` and is reported healthy while it is registered.
- The connection is the liveness. Dropping the `Registration` (or the process ending) closes it,
  and the updater reports the service stopped.
- The updater compares the peer's user id (`SO_PEERCRED`) with the one the service's declaration
  names; a registration from another user is refused (`Refusal::WrongUser`). Each reason the
  updater sends is a variant of `Refusal`; a reason a newer updater adds is `Refusal::Other`.
- `register` blocks until the updater has answered, and gives up with `Error::NoAnswer` after
  `ANSWER_DEADLINE_SECS` (10, the updater's own limit on a line) in all, so a dead or hung updater
  cannot hold a service from starting. The deadline is one for the whole call, the connect
  included: a listener that never accepts and whose backlog is full, the write of the line and an
  answer that comes a byte at a time each wait only for what is left of it, and so does a try
  made again after a signal interrupted one. A service that
  registers on its start path therefore waits at most the deadline (10 s) when the updater hangs;
  one that cannot wait that long registers from a thread of its own.
  An answer cut short before its newline is `Error::BadAnswer`, never success.
  A line over `MAX_LINE` bytes is refused
  before anything is sent (`Error::TooLong`).

The crate depends on `rustix` (to give the socket its timeout before it connects), `serde_json`
and `thiserror`, and on no crate of this workspace, and its name
does not say `kbf`; `crates/kbf-integration-test/tests/layering.rs` keeps both true. The server
is `kbf-step-runner::register`, whose tests (`tests/register_client.rs`) run this client against
it over a unix socket.
