# One output object per action, written in one pass

The object is a **ZIP64 archive, uncompressed**: every entry is method 0 (stored), so each file's bytes are contiguous at its
offset, as in tar. The writer has no compression code path and the reader refuses an entry of any other method.

Status: **DESIGN, then built in the same pull request** (branch `task-output-one-pass`). Owner approval of the direction
(2026-11-15): one object per action, outputs read once, "the single pack per action". Linux job mode with a put directory
(`--stage-and-commit`) only; the presigned mode is untouched. Linked from [actions-as-jobs.md](actions-as-jobs.md)
section 5.3 and [jobs-on-jobscheduler-and-tierstore.md](jobs-on-jobscheduler-and-tierstore.md).

## 1. The problem

A put-directory run today (`crates/kbf-task`, `out_dir.rs`) works like a presigned run, which had to be planned before any
output was known:

1. `kbf-outputs` reads and hashes every file, and builds the Directory and Tree records (first read).
2. `kbf-task` plans the layout (`PackLayout::plan`, `plan.rs`): at most 8 packs of 128 MiB and 16,384 records, at most 8
   large objects for blobs over 64 MiB, the manifest as record 0 at offset 0.
3. It reads every file again into the pack and checks each digest (second read), then writes the footer.

The caps exist because a presigned URL has to be minted for a key before the outputs are known. A put directory has no such
need. So: a run whose outputs pass 128 MiB or 16,384 files writes several objects, an output over 64 MiB is a separate
object the frontend copies into the CAS, and every large file is read twice.

## 2. The target

- One object per action: `pack` in the put directory. No second pack, no large object, no record cap, no size cap
  (AGENTS.md B16).
- Each output file is read once. Its bytes are written to the object while they are hashed.
- The manifest and the file index come after the last blob, as members, with the central directory and the end records, found
  through a fixed trailer in the last 138 bytes.
- The frontend reads the last 138 bytes, then the manifest, the file index and the central directory in one ranged read, records every blob (large ones too) as
  `digest -> (object key, offset, length)` in its pack index, and builds the ActionResult as today.

## 3. The format: an uncompressed ZIP64 archive

```text
entry*     local header (30 + name + 20 bytes: flag bit 3, method 0, ZIP64 extra field, sizes and CRC-32 zero)
           the file's bytes (stored)
           data descriptor (24 bytes: signature, CRC-32, compressed size u64, size u64)
entry      .kbf/manifest.json      the TaskManifest JSON, packs 1 and no large object
entry      .kbf/index.json         the neutral file index (below; may be empty)
central directory                  one entry per distinct blob, and the two members
ZIP64 end of central directory     44 bytes, then 40 bytes of extensible data: the trailer below
ZIP64 end of central directory locator
end of central directory           22 bytes, no comment, counts and offsets saturated to the ZIP64 record
```

- **Stored, always.** Every local and central entry says method 0; the writer cannot write another. A reader refuses an
  entry of method 8 (or any) as a bad object (`ObjectError::Method`, ingest `Malformed`).
- **Names.** A blob's digest is known only after its last byte is read, and a ZIP local header comes first and names the
  entry. So a blob is named by a counter, `b/<16 hex digits>`, in its local header and its central entry, and its SHA-256 is
  carried in the central entry's **extra field with the private header id `0x4B42`** ("KB"; 32 bytes; not a PKWARE-registered
  id, which every reader that does not know it skips). The manifest and the index are ordinary stored members with the fixed
  names `.kbf/manifest.json` and `.kbf/index.json`, placed last before the central directory (a standard tool can extract
  them; they are not carried in the directory, which keeps the directory about blobs only).
- **Offsets.** Every local header is exactly `30 + name + 20` bytes (the 20 is the ZIP64 extra field), so a blob's bytes start at
  its header's offset plus that: a reader computes the span from the central directory alone, without reading a local header. A
  lie is caught as for any put-directory record: the first read hashes it.
- **The trailer** is the extensible data of the ZIP64 end record: `magic "KBFTASK1" | manifest_offset u64 | manifest_len u64 |
  index_offset u64 | index_len u64`, offsets of the members' *data*. The last 138 bytes of the object (end of central
  directory 22, locator 20, ZIP64 end record 96) therefore say where the manifest is and where the central directory starts,
  with no scan for a comment. One ranged read from `manifest_offset` to the end then holds the manifest, the index and the
  central directory.
- **Reader rules** (before it trusts the directory): the three end records' signatures, magic and fixed fields; the offsets add
  up to the object's length; both members present once, at the offsets the trailer gives, their CRC-32 right; every other entry
  stored, named `b/...`, with a SHA-256, of size at least 1, inside the records and overlapping no other; no digest twice. Gaps
  between records are allowed (4). The order of blobs carries no meaning.
- **ZIP64 throughout**: a 32-bit size or offset appears nowhere (the plain fields hold `0xFFFFFFFF` and the ZIP64 extra field
  has the real ones), so nothing caps a file or the object.
- Tools: any ZIP reader lists and extracts it (the test `a_standard_zip_reader_lists_and_reads_every_entry_stored` runs the
  pure-Rust `zip` crate over an object, which also checks each CRC-32). The names are counters, so a listing shows `b/...`, not
  paths; the paths are in `.kbf/index.json`. Info-ZIP's `unzip -l` and `-t` read the central directory too; tests in this
  repository do not spawn them.

### Why ZIP, and not tar, EROFS or squashfs

- **Tar** has no end index: finding a file means walking headers (a 512-byte header and padding per entry, 2,000 small files
  cost about 1.5 MB and as many reads), its header needs the size before the data, and its pax SHA-256 can only precede the
  data, which one pass cannot know. ZIP's central directory is the index at the end that a one-pass writer wants, the data
  descriptor puts size and CRC after the data, and the SHA-256 rides in the directory.
- **EROFS and squashfs** are filesystem images: their writers lay out an inode table and directory blocks from the whole tree
  before the data (or need a second pass to place them), they are built for mounting, which this object is not used for
  (the input tree is composed from digests, section 3 below), and a reader needs a kernel mount or a library per format. A
  ZIP is a flat list of spans, which is what the pack index records.
- **Our own format** (the earlier revision of this document) did the same job with a trailer of our own. ZIP is that, with the
  standard's names for the parts and every reader already written. The cost is a 68-byte header and 24-byte descriptor
  per blob and a 128-byte central entry, against 52 bytes before; the measured object below is 0.6 MB longer for 2,000 files on 170 MiB, the file index included.
- **Not compressed.** Build outputs are mostly already-compressed or hashed-for-reuse objects, the frontend reads a blob by
  byte range at a known offset, and a decompressing reader would turn a ranged read into a scan.

### The neutral file index (the `.kbf/index.json` member)

The object carries one index in no consumer's format. The manifest already has the exit code, stdout, stderr, the timing
and the top-level output files and symlinks; the index adds, for every output file and symlink **including
those below an output directory**, its full path, size, executable bit, SHA-256 and the offset of its bytes in this
object, and every directory (so an empty one is not lost):

```json
{"v":1,"files":[{"path":"out/d/z","size":3,"exec":false,"sha256":"<hex>","offset":20}],
 "symlinks":[{"path":"out/d/l","target":"z"}],"dirs":["out/d","out/d/sub"]}
```

Each list is sorted bytewise by path; `offset` is absent for the empty file (no record). The task builds it from the Trees
it wrote itself (read back from its own file after a flush: they are small records of its own, not outputs) and the
manifest. It is a list of facts about this object, and nothing in it is tierstore's or REAPI's.

**Layering.** Two systems name the same folder, and they number it differently:

| | REAPI / Buck2 | tierstore |
|---|---|---|
| a folder | `Directory` proto, digest = SHA-256 of the proto | tree manifest v1 JSON, digest = SHA-256 of the manifest |
| a file | SHA-256 and size | SHA-256 and size, plus `source` = {key, offset} |

The two folder digests are different numbers for the same folder; the file digests are the same. kbf-task writes neither
format into the object. **The frontend is the only translator**: from the manifest and the file index it derives (a) the
REAPI ActionResult and the Directory and Tree records (the Buck2 side), and (b) tierstore tree manifests, with `source` =
{this object's key, offset}, when it publishes an input tree (`crates/kbf-front/src/action_tree/encode.rs`). The key is the
frontend's own (it minted it), which is why the task could not write a tierstore manifest even if it were to: the manifest's
digest covers the key. Because (b) uses the frontend's own encoder, a manifest built from the index has the bytes, and so
the digest, the frontend would have built from the Trees: publishing stays deduplicated against any earlier publication. A
reused subtree (a later action's input equal to an earlier output directory) stays cheap because the frontend memoises the
manifest it built for each output directory (`action_tree/memo.rs`), so only newly composed top-level directories are
written. The frontend still builds the ActionResult from the manifest and the footer in the one ranged read of section 5.

**Why whole-tar mounting of dependencies is not used** (mounting each dependency's output, as a tar or one overlay layer,
into the action's root):

- Buck2 composes an input root from parts of many outputs at paths it chooses; a dependency's output is rarely mounted
  whole, at the place it was written.
- Hermeticity: an action must not see a file it did not declare, and a whole-output mount shows every undeclared sibling.
- Overlay layers have a limit and an action may have hundreds of dependencies.

So the input tree is composed per action from digests, and the object is only where file bytes live: each manifest entry
points at a span of it.

**What the frontend does with it** (`kbf-front`, `output_index.rs`, `untrusted/one_pass.rs`,
`TreeWriter::publish_output`). At ingest, after the manifest and the records are checked:

1. **The index must agree with the manifest.** Every output file and symlink of the manifest is in the index with the same
   digest, mode and target; the `Directory` and `Tree` messages derived from the index for each output directory (names
   sorted bytewise, a directory's files, directories and symlinks each in that order, the `Tree`'s children in the pre-order
   the walk of `kbf-outputs` opens them) have **exactly the Tree digest the manifest names**, else `BadManifest`. The derived
   Tree is byte for byte what `kbf-outputs` builds from the directory (the test
   `the_tree_derived_is_the_one_kbf_outputs_builds_from_the_directory` runs the real collection over a tree with nested,
   empty and executable entries). Every non-empty file the index names must be a record of the object, else `Incomplete`.
2. **The ActionResult** is built from the manifest as before; the index agreeing with it is what makes the Trees the task
   wrote in the object trustworthy without reading them.
3. **The tierstore side.** Given the frontend's `TreeWriter`, each output directory's `Directory` records are turned into
   tierstore manifests bottom-up by the encoder that builds action trees (`action_tree/encode.rs`), each file child with
   `source` = {this object's key, the offset of its record}, stored create-only at `cas/l/<sha256 of the manifest>` and
   **remembered in the writer's memo under the `Directory` digest**. A later action whose input root holds a subtree equal to
   that output directory (same `Directory` digest) finds the manifest in the memo and writes nothing for it; only the newly
   composed top-level directories are written. The manifest digests are the ones the frontend would publish itself, since it is
   the same encoder over the same facts, so publishing stays deduplicated against any earlier publication. A store that
   refuses the write is logged and costs the reuse, never the run (`a_manifest_that_cannot_be_published_costs_the_reuse_and_not_the_run`).
   The memo is the existing one: bounded, expiring after ten minutes, forgotten by input root.

All of it exists only for an object the gate (`--task-out-format one-pass`, the default since the release after v0.0.83) made a task write; an object of
the packs format is read as before and nothing is derived. The tail bounds of 5.1 are config as well: the manifest cap
(`--task-max-object-manifest-bytes`, default 4 MiB) and the allowance of a record (`--task-object-bytes-per-entry`, default
1024), with `--task-max-object-entries`.

**Large files inside an output directory.** In the packs format a file over the record cap inside an output directory was
refused at the manifest check (`large slot 0 ... no output names`). Main's #2330 fixed that for the packs format (the validator
no longer requires a large entry to be an output's digest; the frontend refuses one that is neither an output's nor a file of
an output directory's tree), and this branch, merged with it, passes that test
(`a_directory_output_with_a_file_over_the_record_cap_packs_and_validates`,
`a_large_object_inside_an_output_directory_is_stored_and_the_run_cached`). A one-pass object has no such case: a large blob is a
record like any other.

## 4. The one-pass algorithm, and dedup

`kbf-outputs` walks the outputs one file at a time. With a **tee** installed (`Store::tee`, default none: every other user
of the crate is unchanged) a file is handled by size:

- At most 1 MiB (`CHUNK_BYTES`): read whole into memory, hashed, then **written only if its digest is new**. No waste.
- Larger: each chunk is written to the object as it is read and hashed; when the file ends its digest is known. If the
  digest was already written, the bytes just written are **dead**: the writer does not index them and does not move on from
  them. They stay in the object. This is the one tolerated waste, and it is the whole of it: a duplicate of an earlier
  file larger than 1 MiB. We tolerate it and do not truncate because the put directory is a write-once stream of the
  node's store and a `set_len` on it is not something tierstore promises. The writer counts them (`Written::dead_bytes`); a
  build whose outputs repeat large files pays their size once more in the object (never in the CAS: dead bytes are in no
  index entry).
- Directory and Tree messages and stdout/stderr go through the same writer (`Store::put`), deduplicated before they are
  written.
- After the walk: the manifest and index members, the central directory and the end records. The object's own SHA-256 is taken as the bytes go out, for the store's
  `<name>.sha256-<hex>.part` rename. Then the commit rename, as today.

The only reads of an output are the one that hashes and writes it. A file that changes while it is read is not detected by a
second read; its bytes are hashed as they are written, and the digest is that of the bytes in the object, so the object is
always self-consistent. (The Command has ended and the sandbox hid the out directory; this matches what `kbf-outputs`
already says of files changing during the walk.)

## 5. The frontend

`ingest_put_dir` reads the last 138 bytes of `pack`. If they are the end records of a one-pass object (three signatures and the magic) it follows 5.1; otherwise the old path runs
unchanged (the same bytes are reused for its 32-byte trailer, so an old object costs no extra request).

5.1 One-pass object: parse and check the end records (entry count at most `max_object_entries`, a config key,
`--task-max-object-entries`, default 1,048,576; manifest at most 4 MiB; the range at most the manifest cap plus 1 KiB per
entry; all before anything is read); read `[manifest_offset, end)` once; check the members' CRC-32s and the rules of 3 (an entry that is not method 0 is `Malformed`); parse the manifest
(`packs` must be 1, `large` empty, for this lease and action; the declared-outputs check is unchanged); every digest the
manifest names must be an entry (else `Incomplete`); record the object in the pack index with one row per entry, large
ones included. Blobs over `max_blob_bytes` are then stored as today for a frontend whose reads go to storage nodes, by a
ranged read of the object (`Large` with a base offset); a frontend that serves large blobs from the index
(`Packs::read_large`) needs nothing more, since the row says where they are. The action-cache write is last, as before.

The proof is the lighter one of a put directory (`Proof::Manifest`): a record whose bytes are not its digest is dropped by
the first read that hashes it. Nothing about that changes.

## 6. Compatibility (AGENTS.md B8)

Additive: a new object kind under the same key, a new flag, no field of any shipped format changes.

- **Reader first.** The frontend reads both formats from the release that adds this. It does not need telling which.
- **Writer gated by the frontend.** `--task-out-format` (config key `task-out-format`) was `packs` before the
  one-pass release and `one-pass` after it (default); the packs writer is now removed. The key is still accepted with its
  one value, so a config that pins `one-pass` starts, and the executor still adds `--object-format one-pass` to the
  task's arguments, which `kbf-task` still accepts (it has no other format), so a frontend of the release before and one
  of this release both work with this task. A config that sets `packs` is refused at start.
- **Rollout.** (1) Ship the release to every frontend and the base tree that holds `kbf-task`. (2) The default is
  `one-pass` since that release; the packs writer was removed one release after it. Rollback: roll the config back
  before the binary; objects already written stay readable by the new frontend, and a frontend rolled back to a release
  before the one-pass object refuses a one-pass object. The ingest still reads packs and large objects (a run of the old
  task in flight, stored objects), so no data refused.
- **The test** that runs the previous release on new data and the reverse: `one_pass_and_packs_ingest_side_by_side` (frontend
  reads an old pack and a one-pass object in the same farm).

## 7. Failure modes

| Failure | Result |
|---|---|
| A file cannot be read or the object cannot be written | task exit 12/13 as today; the `.part` file is removed; nothing committed |
| Commit rename fails | the retries of today's `commit` |
| A task is killed mid-write | a `.part` file the store's sweep removes; no `pack`, so ingest answers `Missing` |
| A duplicate large file | dead bytes in the object (4), not an error |
| A lying object (a directory that claims bytes that are not the digest, overlapping entries, entries past the manifest, an entry of method 8) | refused by the rules of 3 as `Malformed`, or served until the first read hashes the record, as for any put-directory pack |
| A directory with more than `max_object_entries` entries | `TooLarge` before it is read |
| The old frontend meets a one-pass object | `Malformed`; a configuration error caught by 6's order |

## 8. Results

Every number below says whether it was **measured** (read from the run) or **inferred** (computed from measured numbers or
from the code).

**Setup (measured).** One Linux host (kernel 6.17), release build, page cache warm, no sandbox (the host refused the Command's
namespace step: `sandbox: incomplete (setgroups 13)`, the same for both formats). A throwaway harness (not committed) ran
`kbf_task::tree::execute_out_dir` with a plain directory as the put directory and a tree job whose Command wrote one 170 MiB
file of random bytes (`big`) and 2,000 files `d/f0..f1999` of 1,000 to 2,999 random bytes, outputs `big` and `d`. Three rounds,
the two formats alternating (packs first), each in a fresh tree. Bytes read are `rchar` of `/proc/self/io` read before and after
the call. The task's wall time minus the Command's own wall time (`timing.run_ms`) is the task's overhead for the action: the
input read, the collection, the write and the commit.

| | packs (`--object-format` absent) | one-pass |
|---|---|---|
| objects committed (measured) | `pack` and `L0` (the 170 MiB file is a large object) | `pack` |
| bytes read, `rchar` (measured, all three rounds identical) | 545.5 MiB | 375.6 MiB |
| task overhead per action, ms (measured, three rounds) | 380, 257, 272 | 230, 227, 236 |
| median task overhead, ms (measured) | 272 | 230 |
| bytes of the objects (measured) | 182,680,914 | 183,289,278 |

**Bytes read.** The difference is 169.9 MiB (measured: 545.5 - 375.6), one read of the 170 MiB file, as the design says: the
packs format reads each output twice, one-pass once. Both totals also hold what the Command and its children read, which
`/proc/self/io` counts once they are reaped: 170 MiB of `/dev/urandom` for `big` and about 36 MiB for the shell and the 2,000
`head` runs (inferred from a run with a 1 MiB `big`, which read 38.5 MiB with packs and 37.6 MiB with one-pass; the inference
is that the Command's share is the same for both formats). The outputs' own share is then about 340 MiB with packs and 170
MiB with one-pass (inferred), plus the small files, which both formats read once.

**Task overhead per action.** One-pass is faster by 42 ms at the median (measured: 272 - 230), and by 27 to 150 ms across
rounds (measured pairs: 380 vs 230, 257 vs 227, 272 vs 236); the first packs round (380 ms) is slower than its later two, which I take (inferred) to be a cold start of the harness's
files, so the median is the figure to quote, not the mean (303 against 231 ms, a 72 ms difference, measured). The gap is the
second read of the 170 MiB file from a warm page cache plus the second object (the large object's separate create, write and
rename); on a cold cache or a slower disk the second read costs more (inferred; not measured here). The output phase is
about 0.25 s of a task of about 3.9 s in this setup, so the share saved is small for this Command and grows with output size.

**Object size.** One-pass is 608,364 bytes longer (measured; 0.33%): the ZIP64 local header (68 bytes for a 18-byte name) and
data descriptor (24 bytes) of each of 2,002 blobs, their central directory entries (about 128 bytes each), and the file index
(about 150 bytes a file). The split of the 608,364 between those is inferred from the format, not measured separately.

**Not measured.** A cold cache; a put directory on the node's tierstore (the harness wrote to a plain directory, so the
commit rename cost of the real store is in neither column); several actions at once; the frontend's ingest time for the two
formats; memory use of either writer.
