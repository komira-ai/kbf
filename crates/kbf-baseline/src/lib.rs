//! Client-side numbers for a build farm, from buck2's own event logs: what ran where
//! (on the farm, from its action cache, locally), how long the build and each action
//! took, how long the farm queued and executed each action, the cache hit share, and
//! which actions failed. It reads only the log the client wrote, so a build against
//! one remote-execution farm and the same build against another give numbers that can
//! be compared line by line.
//!
//! # The input
//!
//! buck2 writes one event log per command. Under `buck-out/v2/log` it is length-delimited
//! protobuf compressed with zstd (`.pb.zst`); given `--event-log FILE`, it also writes the
//! same events to `FILE` in the encoding the extension names. This crate reads the JSON
//! encoding, `FILE.json-lines` or `FILE.json-lines.gz`: the first line is the invocation
//! header, every other line one `StreamValue` (`{"Event": ...}`, `{"Result": ...}`).
//! `buck2 log show [PATH | --recent N]` prints a stored log in the same encoding, so a
//! build that already ran converts with `buck2 log show ... > FILE.json-lines`.
//!
//! The field names are those of buck2's `app/buck2_data/data.proto`, as its serde
//! encoding writes them (durations as `<name>_us` integers, enums as their numbers,
//! a `oneof` as an object keyed by the variant). They were read from the buck2 release
//! CI pins (`BUCK2_VERSION` in `.github/workflows/ci.yml`), and the tests run on logs
//! that release wrote in CI ([`read`] says what each number comes from).
//!
//! # The numbers
//!
//! [`report`] turns one or more builds into a [`report::Report`]: per build and for the
//! whole set, the actions by [`read::Ran`], the cache hit share, the nearest-rank p50,
//! p90 and p99 and the maximum of the action, queue and execution times, and every
//! failed action. The binary prints it as a table and, with `--json`, as JSON.

//!
//! # Recording a baseline
//!
//! Build with an event log per build, then report on the set:
//!
//! ```text
//! buck2 build --event-log logs/build-1.json-lines.gz //...
//! buck2 clean && buck2 build --event-log logs/build-2.json-lines.gz //...
//! kbf-baseline --json baseline.json logs/*.json-lines.gz
//! ```
//!
//! The same commands against another farm give the second set to compare.

pub mod read;
pub mod report;
