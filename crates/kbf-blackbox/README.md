# kbf-blackbox

The black-box suites of the beta stage (design: T009). Tests only: the crate has no
library and no binary, and every dependency is a dev-dependency. A test starts the real
`kbf` processes through `kbf-harness` and talks to them as a deployer's client would
(REAPI, the operator API, the command line); it links no `kbf-server` or `kbf-node` code.

## Suite A: all-in-one, end to end

`tests/all_in_one.rs` (test binary `kbf-blackbox::all_in_one`) holds suite A: a farm of
rendered units (a frontend, storage-and-daemon nodes, a plain daemon) driven end to end.
Today it holds one test, `a_storage_node_takes_a_lease_and_runs_an_action`: an `Execute`
reaches a node's daemon half, the action runs under the stand-in `crun`, and its output is
read back through the frontend. It is the proof that the wiring works; the planned-restart
test of `kbf-cli/tests/rehearsal_in_a_box.rs` moves here next.

## Suites 1 and 2: no residue, no cross-action data access (T018)

`tests/isolation/main.rs` (test binary `kbf-blackbox::isolation`) holds both, in
`no_residue.rs` and `no_cross_access.rs`, over one set of helpers (`common/`). Linux legs
only; the Mac legs are jerry's (see "The Mac seam").

### What a green run proves, and what it does not

The farm's `crun` is a STAND-IN (crates/kbf-driver-container/tests/bin/fake_crun.rs): it
makes no user, mount, pid or network namespace, no overlay and no kernel cgroup, and runs each
action as a plain process of the test's user; its cgroup tree is plain directories; it ignores
the REAPI Command and runs the one planted action (`Rig::plant`). So these suites are tier S,
"layout": they prove what the PRODUCT's own code creates, cleans, hands over and collects
(`kbf-daemon`, the container driver, the keeper, the output collector, the input
materialiser). They never prove kernel isolation. A green run is not "no cross-action access"
on a real node; it is "the product's layout and bookkeeping give no path to it".

Tier R is the same questions on real rootless `crun`: `#[ignore = "needs real rootless crun:
cargo xtask crun-tests"]` tests in both files that FAIL LOUDLY with the reason when run on
the stand-in (`--include-ignored`), so a missing leg is never a skip. They need a farm (or a
daemon-only harness) on real `crun` in a `Delegate=yes` unit (komira-ai/kbf#936). One lease
on real `crun` is already covered by kbf-driver-container's `tests/oci_crun.rs`.

### Checks, and the planted leak that turns each red

Every probe is a function returning `Err(what)`; the `planted_` test beside it makes a
deliberately leaky action (or switches on a stand-in `crun` knob) and asserts the SAME probe
returns `Err`. No production code has a knob; plants are `kbf-act` steps run by the stand-in
and its existing knobs.

| check (file) | planted leak |
|---|---|
| lease dirs, crun state (below the lease), lease cgroups, new paths, processes, zombies gone the moment `Execute` returns (`successful_runs_leave_nothing`) | `planted_lease_directory_is_seen`, `planted_cgroup_directory_is_seen`, `planted_failing_delete_leaves_cgroups_that_are_seen` (knob `delete-fails`), `planted_stray_file_is_seen`, `planted_escaping_process_is_seen` (setsid), `planted_zombie_is_seen` |
| previous lease gone before the next runs, fresh upper (`the_previous_lease_is_gone_before_the_next_one_runs`) | shares the lease-directory plant |
| failed / timed-out action leaves nothing | shares the plants above |
| children in the action's group are killed | `planted_escaping_process_is_seen` |
| disk: non-kept scratch exactly as before (slack zero), cache +input once, stores bounded, same input adds nothing (`runs_do_not_grow_the_node_beyond_what_it_keeps`) | `planted_growth_of_an_existing_file_is_seen`, `planted_large_stray_file_is_seen_with_its_size`, `planted_runaway_store_growth_is_seen` |
| kept-path allow-list is exact, `runs/` and `quarantine/` are residue | `planted_lookalikes_of_kept_paths_are_seen`, `planted_runs_and_quarantine_entries_are_seen` |
| a walk or read error fails, a missing area fails | `planted_unreadable_directory_fails_the_probe`, `planted_missing_lease_area_fails_the_probe` |
| daemon SIGKILL + restart: sweep removes the old lease (`a_restarted_daemon_sweeps_the_lease_it_left`) | `planted_stray_file_survives_a_restart_and_is_seen` |
| B after A cannot name A's data; A's canary in no file outside the stores (`a_later_action_finds_nothing_of_an_earlier_one`) | `planted_shared_file_between_actions_is_seen` |
| output collector does not follow links (`outputs_that_are_links_are_not_followed`) | `planted_output_with_a_neighbours_bytes_is_seen` |
| input links stay links, `..` names refused (`input_links_and_dotdot_names_do_not_escape`) | `planted_escape_file_and_read_through_link_are_seen` |
| action inherits only fds 0, 1, 2 (`the_action_inherits_only_its_standard_descriptors`) | `planted_inherited_descriptors_are_seen` |
| live lease not writable by others, input files read-only | `planted_world_writable_lease_is_seen` |
| result cached only for the identical action | the identical action is the plant, inside the test |

### Not tested on the Linux stand-in (and why)

- Anything that needs a namespace, a uid, a mode enforced across ids, an overlay or a kernel
  cgroup: B reading A's LIVE files (concurrent leases), pid and network namespaces,
  copy-up of input links (X6; the stand-in's links are symlinks you can write through),
  `/dev/shm` and `/tmp` being the lease's own (the stand-in's are the host's), a cleared
  environment (the stand-in does not `env_clear`), `cgroup.kill` reaching a setsid'd
  escapee or a killed daemon's action. Tier R tests exist as failing-loudly stubs.
- Two leases in flight on the stand-in: it keeps ONE record of the last action for the node
  (`pid`, `root`, `upper`), so a second lease would signal the first. A and B are sequential.
- The sweep's removal of a live lease cgroup: the stand-in's plain directories make it
  ENOTEMPTY; the restart test removes the dead cgroup first (documented in the test).
- Cancel/kill through the operator API, the input cache's LRU eviction under a small budget,
  `/tmp`-style host plants (never run here: they would touch the real host), network plants
  (they need new `kbf-act` steps and the stand-in shares the host's loopback).

### The Mac seam

Tests call `common::node::NodeView` and the probes built on it, never `/proc` or cgroup paths.
Jerry's Mac legs add `MacNode` implementing it (per-slot users, simulators, `whole-machine`
lease directories, launchd jobs, the UI session) where the tests pick `LinuxNode`, plus a
`Rig` that starts a Mac farm; the two files then run unchanged. The Mac-shaped methods are
already on the trait (`slot_users`, `session_leftovers`, `home_dirs`, `isolation`), empty on
Linux; `Isolation::PerUid` tells the cross-action probes that a live read of another
action's file must be DENIED by uid and mode, where on the Linux stand-in it must succeed
(`Isolation::Nothing`) and on real `crun` it must not even resolve (`MountNs`). A Mac method
not written yet must `unimplemented!("mac leg: ...")`, so a missing leg is loud.

### Running

`cargo build -p kbf-cli --bins` first (the farm's processes are found beside the test
binary), then `cargo test -p kbf-blackbox --test isolation`. Each test starts its own farm
(ports and directories are the farm's own, nothing is shared between tests), so tests may
run side by side; process scans are keyed on the test's farm directory.

## How to add a test

1. Put it in `tests/all_in_one.rs` (a new suite is a new file in `tests/`, named for the
   suite). Start the farm with `start(name, topology)`, wait with `ready`, drive it with
   the helpers of the file, and `farm.stop()` at the end; a failed `Farm::start` panics
   with the processes' last lines.
2. Say in the test's doc comment which defect it catches, and plant that defect once to see
   it red.
3. Keep the file under 1000 lines; a second suite goes in its own file.

## Where it runs: beta, not the pull-request gate

- `.config/nextest.toml`: the `pr` profile's `default-filter` excludes `package(kbf-blackbox)`;
  the default profile (the whole run of `.github/workflows/beta.yml`, job `test-x86`) runs it.
- Coverage: the pull-request job measures every crate at 99% under `cargo llvm-cov`. The
  test files here are `#![cfg(not(coverage))]`, so that job compiles them empty and the
  crate has no records (an unlisted crate with none passes); a normal build and the beta run
  compile them whole. This crate needs no line in `coverage-floors`.
- The processes are found beside the test binary, so build them first when running one
  package: `cargo build -p kbf-cli --bins` (`kbf` and the stand-in `crun`). `cargo test
  --workspace` and the beta run build them already.
- Linux only: `kbf-harness` is.
