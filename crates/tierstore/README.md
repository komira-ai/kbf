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

## When the puts are at their limit

At most `--max-concurrent-puts` put bodies and upload parts are taken at once. A request that
arrives when all are taken waits up to `--put-permit-wait` (`server.put_permit_wait`, 15s by
default) for one to end, without reading its body, and is taken if one ends in that time. If none
does, it is answered 503 `SlowDown` with `Retry-After: 1` and its body is never read. With a tier
that is slow to take a body, clients queue here for up to that long instead of retrying at once.
`0s` answers 503 at once. The requests that are refused for their head (411, 413) do not wait.

What waiting costs, as the code does it:

- A waiting request holds a connection and its task, and no permit and no body memory. It has
  been authenticated (a valid signature) before it waits. The number of waiters is bounded only by
  the number of connections the server accepts.
- A waiter counts as a request in flight. During a drain the server waits for it, so a drain can
  take up to `--put-permit-wait` longer than without waiting, and never longer than
  `--drain-deadline`.
- A waiter does not notice that its client went away: the connection is looked at again when the
  body is first read, after a permit is given.
- The client of kbf (`kbf-task`) retries the 503, and a connection that broke while it was still
  sending the body of a refused PUT, with jittered waits (see its crate docs). It does not retry
  429, 500, 502 or 504, deliberately.
- The 503 is sent before the body is read, so the front may close the connection under a client
  that is still sending a large body. A client that sees the reset instead of the 503 has to treat
  it as one; `kbf-task` does.

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
