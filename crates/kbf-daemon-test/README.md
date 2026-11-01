# kbf-daemon-test

The black-box suites of the daemon, a beta stage test (design: T009, T018). Tests only: the
crate has no library and no binary, and every dependency is a dev-dependency. A test starts
the real `kbf` processes through `kbf-harness` and talks to them as a deployer's client would
(REAPI); it links no `kbf-server` or `kbf-node` code. One crate per component under test,
named for it (`kbf-farm-test` is the farm's). The Mac legs go here as more
test targets (the first: `tests/mac_pool_spike`, below).

## No residue, no cross-action data access (T018)

`tests/isolation/main.rs` (test binary `kbf-daemon-test::isolation`) holds both suites, in
`no_residue.rs` and `no_cross_access.rs`, over one set of helpers (`common/`, below). Linux legs
only; the Mac legs are owned separately (see "The Mac seam").

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
| kept directories that must be EMPTY between leases hold nothing (`oci-helper-state`, any size); the node directory outside `data/` (config, keys, id maps, binaries) is exactly its expected list and nothing grows (`kept_directories_hold_no_job_leftovers_and_the_node_directory_is_unchanged`) | `planted_leftover_in_the_helper_state_directory_is_seen`, `planted_residue_outside_the_data_dir_is_seen` (incl. lookalikes of the exempt names) |
| orphans: found by the farm path in cmdline, environment or cwd, by the process group of a farm role, by the group or session of the last action, by `cgroup.procs`, and by parentage (`processes_of`) | `planted_hidden_orphans_are_found_by_group_session_and_cgroup`, `planted_hidden_orphan_in_the_daemons_group_is_seen` |
| kept-path allow-list is exact, `runs/` and `quarantine/` are residue | `planted_lookalikes_of_kept_paths_are_seen`, `planted_runs_and_quarantine_entries_are_seen` |
| a walk or read error fails, a missing area fails | `planted_unreadable_directory_fails_the_probe`, `planted_missing_lease_area_fails_the_probe` |
| daemon SIGKILL + restart: sweep removes the old lease (`a_restarted_daemon_sweeps_the_lease_it_left`) | `planted_stray_file_survives_a_restart_and_is_seen` |
| B after A cannot name A's data; A's canary in no file outside the stores, and in the input cache and trees only in a file named for the digest of its bytes (`a_later_action_finds_nothing_of_an_earlier_one`) | `planted_shared_file_between_actions_is_seen`, `planted_copy_in_the_input_cache_is_seen`, `planted_digest_named_copy_with_other_bytes_is_seen` |
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
- An orphan that left the farm's process groups and sessions (`setsid`), cleared its
  environment, went to `/` and was reparented to init: no probe here can attribute it to the
  farm (the stand-in's cgroups have no `cgroup.procs`, and a box shared with other users'
  processes rules out "every new process of the user"). Tier R stub
  `tier_r_an_orphan_that_hid_is_found_by_its_cgroup`.
- `children_in_the_actions_group_are_killed` proves the group is gone, not that the PRODUCT's
  `cgroup.kill` ended it: the stand-in's `crun delete --force` kills the action's group too
  (dropping the product's explicit `cgroup.kill` leaves this test green). Tier R.
- Two leases in flight on the stand-in: it keeps ONE record of the last action for the node
  (`pid`, `root`, `upper`), so a second lease would signal the first. A and B are sequential.
- The sweep's removal of a live lease cgroup: the stand-in's plain directories make it
  ENOTEMPTY; the restart test removes the dead cgroup first (documented in the test).
- Cancel/kill through the operator API, the input cache's LRU eviction under a small budget,
  `/tmp`-style host plants (never run here: they would touch the real host), network plants
  (they need new `kbf-act` steps and the stand-in shares the host's loopback).

### The Mac seam

Tests call `kbf_testkit::node_view::NodeView` and the probes built on it
(`kbf_testkit::probe`), never `/proc` or cgroup paths. Both live in `kbf-testkit`, so a Mac
leg imports them like any other crate: no `#[path]` include. The Mac legs add a test
target to THIS crate (`tests/<name>/main.rs`) with a `MacNode` implementing the trait
(per-slot users, simulators, `whole-machine` lease directories, launchd jobs, the UI
session) where the isolation suites pick `LinuxNode`, plus a `Rig` that starts a Mac farm;
the checks then run against it unchanged. The Mac-shaped methods are already on the trait
(`slot_users`, `session_leftovers`, `home_dirs`, `isolation`), empty on Linux;
`Isolation::PerUid` tells the cross-action probes that a live read of another action's file
must be DENIED by uid and mode, where on the Linux stand-in it must succeed
(`Isolation::Nothing`) and on real `crun` it must not even resolve (`MountNs`). A Mac method
not written yet must `unimplemented!("mac leg: ...")`, so a missing leg is loud.

### Where the helpers live

| helper | crate | why |
|---|---|---|
| `NodeView`, `Isolation`, `Proc`; the probes over files (`walk`, `Snapshot`, `clean_since`, `scan`, `files_containing`, `world_writable`, ...); `canary`, `noise`; `Tree`, `Job` | `kbf-testkit` (a re-export of `js-testkit`) | no protocol and no platform in them, and `kbf-testkit` takes no kbf crate (every crate may depend on it); unit-tested in `js-testkit/tests/` in the jobscheduler repository |
| the REAPI channel, `upload_action`, `execute` | `kbf-harness` (`reapi`) | they name `kbf-proto` and `tonic`; shared with `kbf-farm-test`; unit-tested in `kbf-harness/src/reapi_tests.rs` |
| `LinuxNode`, the `/proc` and cgroup probes (`processes_of`, `open_fds`, ...), `Rig` (one node, the stand-in `crun`'s knobs) | here, `tests/isolation/common/` | Linux-only or specific to this farm; part of a test binary the pull-request gate does not run |

### Running

`cargo build -p kbf-cli --bins` first (the farm's processes are found beside the test
binary), then `cargo test -p kbf-daemon-test --test isolation`. Each test starts its own farm
(ports and directories are the farm's own, nothing is shared between tests), so tests may
run side by side; process scans are keyed on the test's farm directory.

### The Mac spike (`tests/mac_pool_spike`, M0 of the Mac legs)

A root-only, macOS-only test target that asserts NOTHING about isolation. It makes two pool
users `kbf-pool-0` and `kbf-pool-1` with `dscl` (uids from 790, the primary group id the first
uid, as `Pool::provisioned` mandates), starts the real `kbf daemon --driver native
--pool-users 2 --pool-first-uid 790` as root behind the fake front kbf-cli's
`tests/macos_daemon.rs` uses, runs actions through it, and writes every fact as a `SPIKE` line
(`grep '^SPIKE'` reads the log back; the last block, `SPIKE summary`, answers the questions):
does `launchctl asuser` work for `dscl`-made users, does `user/<uid>` exist with nobody logged
in, does a root daemon with a pool start on the runner, did the action run as a pool uid, do
`security`, `pbcopy`, `pbpaste` and `defaults` work for that user headless (one lease per
program, run by the daemon and, without daemon and sandbox, by `launchctl asuser`), did the
lease directory appear while the action ran and is it gone after the `Result`, what two
leases at once look like, and how long each step takes. It fails only when the harness did not
run (users not made, no `Hello`, a lease without a `Result`).

It must run as root and never touches a provisioned pool: every record it makes carries a
mark in its `Comment`, a run first removes the marked leftovers of an earlier run, and it
refuses to start when a `kbf-pool-<n>` user, a user at one of its uids or a group at its
group id is not its own. Run it with `cargo build -p kbf-cli --bin kbf --locked`, then
`cargo xtask sudo-test -p kbf-daemon-test --test mac_pool_spike -- --test-threads=1
--nocapture`. It runs in beta.yml's `native-macos` job (after the pool tests of
`kbf-driver-native`) and, by hand on a branch, as the `pool-spike` job of
`spike-slot-reach.yml`.

## How to add a test

1. Put it in `tests/isolation/no_residue.rs` or `no_cross_access.rs` (a new suite is a new
   test target: a directory `tests/<name>/main.rs`). Start a node with `Rig::start(name)`,
   plant the action with `rig.plant(..)`, run it with `rig.run(&job)`, and look with the
   probes. A failed `Farm::start` panics with the processes' last lines.
2. Say in the test's doc comment which defect it catches, and plant that defect once to see
   it red (a `planted_` test beside it makes the same probe return `Err`).
3. Keep the files under 1000 lines.

## Where it runs: beta, not the pull-request gate

- `.config/nextest.toml`: the `pr` profile's `default-filter` excludes `package(kbf-daemon-test)`;
  the default profile (the whole run of `.github/workflows/beta.yml`, job `test-x86`) runs it.
- Coverage: the pull-request job measures every crate at 99% under `cargo llvm-cov`. The
  test files here are `#![cfg(not(coverage))]`, so that job compiles them empty and the
  crate has no records (an unlisted crate with none passes); a normal build and the beta run
  compile them whole. This crate needs no line in `coverage-floors`. The helpers in
  `kbf-testkit` and `kbf-harness` ARE measured by that job, which is why they have tests of
  their own.
- The processes are found beside the test binary, so build them first when running one
  package: `cargo build -p kbf-cli --bins` (`kbf` and the stand-in `crun`). `cargo test
  --workspace` and the beta run build them already.
- Linux only for the suites above: `kbf-harness` is.
