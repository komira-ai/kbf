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

## Metrics

`GET /metricz` on the S3 listener (not signed; a bucket cannot be called `metricz`) is the
Prometheus text of the counters of the proxy's reads. All are counters since the process started.
The names put the tier in them (`tierstore_hot_...`, `tierstore_cold_...`), the way the local
cache's put `cache` in theirs, and a label has a few fixed values, never a key or a client.

An S3 GET reaches the proxy as a head (for the size and the conditions) and then a get, so a
GET adds one to `op="head"` and one to `op="get"`. A key that exists nowhere is answered at the
head and shows only under `op="head"`.

| name | labels | meaning |
|---|---|---|
| `tierstore_hot_hits_total` | `op` = `get`, `head` | reads the hot tier had the object for |
| `tierstore_hot_misses_total` | `op` | reads the hot tier did not have it for (the read goes on to the cold tier) |
| `tierstore_cold_hits_total` | `op` | reads the cold tier had it for, after a hot miss |
| `tierstore_cold_misses_total` | `op` | reads neither tier had it for |
| `tierstore_hot_bytes_served_total` | none | bytes of object data that gets answered from the hot tier |
| `tierstore_cold_bytes_served_total` | none | the same, from the cold tier (the range, if one was asked) |
| `tierstore_cold_reads_total` | `op` | reads that went on to the cold tier: cold hits, cold misses and cold errors |
| `tierstore_cold_bytes_read_total` | none | bytes gets read from the cold tier: whole objects, whatever range was asked |
| `tierstore_promotions_total` | none | objects a get copied from the cold tier into the hot tier |
| `tierstore_promoted_bytes_total` | none | bytes of those objects |
| `tierstore_promotions_skipped_total` | `reason` = `critical`, `full`, `raced`, `failed` | copies not made: hot tier at its critical mark, hot tier said full, another read copied first, any other failure |

A read that fails with anything but "not found" is counted as neither a hit nor a miss. A head
promotes nothing.

PromQL, over the reads that find an object (`op="get"`):

```
# hot hit rate
sum(rate(tierstore_hot_hits_total{op="get"}[5m]))
  / (sum(rate(tierstore_hot_hits_total{op="get"}[5m])) + sum(rate(tierstore_hot_misses_total{op="get"}[5m])))
# share of gets answered by the cold tier
sum(rate(tierstore_cold_hits_total{op="get"}[5m]))
  / (sum(rate(tierstore_hot_hits_total{op="get"}[5m])) + sum(rate(tierstore_cold_hits_total{op="get"}[5m])))
# cold reads per second, and promotions per cold hit
sum(rate(tierstore_cold_reads_total{op="get"}[5m]))
sum(rate(tierstore_promotions_total[5m])) / sum(rate(tierstore_cold_hits_total{op="get"}[5m]))
# bytes read from the cold tier per byte answered from it (read amplification of ranged gets)
rate(tierstore_cold_bytes_read_total[5m]) / rate(tierstore_cold_bytes_served_total[5m])
# lookups of keys that exist nowhere
sum(rate(tierstore_cold_misses_total{op="head"}[5m]))
```

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
