# tierstore

An S3 front over a hot and a cold tier. It serves one bucket, path-style, and checks SigV4 in the
`Authorization` header (and presigned PUT URLs). The packaging notes, the flags and the key files
are in `packaging/tierstore/README.md`; the roles of a key are in `src/server/keys.rs`.

## Flags or a file

The settings that describe the data path are flags, or one TOML file given with `--config <path>`
(`[server]`, `[hot_store]`, `[cold_store]` and `[[policy]]`; `src/server/config.rs`). The two make
the same settings and run the same code. `--print-config`, given the flags of a start, prints the
file that is equivalent to them. A file holds no secret, only the paths of key files, and refuses
a key or a table it does not know. The format, the table of the key of each flag and the rules are
in "The configuration file" in `packaging/tierstore/README.md`; the flags stay accepted for one
release along with the file.

## A build node: a read cache

A file with `[local_cache]` and `[cold_store]` and no `[hot_store]` runs the same binary as a read
cache in front of one upstream (`src/server/local_cache/`, the file's tables in
`src/server/config_cache.rs`): GET and HEAD of the keys a `[[policy]]` with `cache = true` holds are
served from a directory of raw files (`<path>/obj/<aa>/<bb>/<hex>`), filled from the upstream on a
miss, hashed as they are filled and evicted oldest-use first under a byte budget and a free-space
floor; every other call is refused 403. It has no journal and no lease. The keys of the table and
the rules are in "A build node" in `packaging/tierstore/README.md`; `GET /metricz` shows its
counters.

With `write_through = true` in `[local_cache]` (version 0.2.0 on; `tierstore --version` shows it)
the node also takes create-only puts, presigned PUTs, multipart uploads and deletes, and sends
each to the upstream (`src/server/local_cache/write.rs`) before it answers; it keeps no copy of
what is written. The roles of the keys apply as on a storage node. The start tests the upstream's
create-only and multipart-conditional answers with the self-test, so the upstream key must be one
that writes (exit status 3 if not).

## Roles of a key

| role | flag | may do |
|---|---|---|
| full | `--access-key-file` | everything the front serves |
| read-only | `--read-only-key-file` | get, head, list |
| presigning | `--presign-key-file` | sign presigned PUT URLs for one object each under a prefix |
| GC | `--gc-key-file` | list, head, delete and abort uploads under one prefix |
| scoped | `--scoped-key-file` | read, create and delete under the prefixes of three lists |

### Scoped keys

A scoped key's file is JSON of exactly `access_key_id`, `secret_access_key`, `read_prefixes`,
`write_prefixes` and `delete_prefixes`; the lists may be empty, but not all three, and every prefix
ends in `/`, is not, or under, or over `_journal/` or `_tiering/`, and has neither as a segment.

A call whose key or listing `prefix` has `..` anywhere in it is refused, even inside a prefix.
The server library does not itself force the access key ids of its keys to differ (`ServerConfig`
is plain data); the command line does, and exits 2.

- `read_prefixes`: GetObject, HeadObject, ListObjectsV2 and ListMultipartUploads (a listing's
  `prefix` parameter must be inside one);
- `write_prefixes`: create-only PutObject (`If-None-Match: *` or no condition; `412` if the key
  exists) and the calls of a multipart upload (create, part, complete, abort);
- `delete_prefixes`: DeleteObject.

A scoped key never replaces (`If-Match`) and never copies (`x-amz-copy-source`). Everything else is
`403 AccessDenied`. Two uses:

- the frontend's pack store: `read_prefixes: ["cas/"]`, `write_prefixes: ["cas/f/<writer>/"]`,
  `delete_prefixes: []` (add the writer prefix to the delete list to let the frontend remove its
  self-test object and the object of a failed write);
- build history, `--history-objects 's3://<bucket>/kbf-history?endpoint=...&conditional-put=true'`
  (without the last parameter a retried chunk gets `412` for good): `read_prefixes`
  and `write_prefixes` of `["kbf-history/"]`, `delete_prefixes: []`.

See "Scoped keys" in `packaging/tierstore/README.md` for the table of calls.
