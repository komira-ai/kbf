# Troubleshooting

## Exit statuses

| Status | Meaning | The unit |
|---|---|---|
| 0 | a clean drain, or `--print-config` or `--help` done | stays stopped |
| 1 | a start that failed: a tier that could not be reached, an address in use, a key file refused after parsing, and the flags that depend on each other (see "Messages at start") | restarts after 30 seconds |
| 2 | a flag the parser refused, a file that does not load, a limit of the OCI front, or two keys with one id | does not restart: a restart cannot mend it |
| 3 | a tier answered the start-up self-test wrongly | does not restart: the same store would answer the same way |

Messages go to stderr, prefixed `tierstore: `, and so to the journal: `journalctl -u tierstore`. None carries a secret.

## Messages at start

Flags that depend on each other. These are checked after the flags and the file have parsed, and a start that fails one exits with status 1, not 2, so the unit starts again every 30 seconds until the value is fixed (a file's message names the key where a flag's names the flag):

```message
--low-water must be below --high-water
--high-water must not be above --critical
--reconcile-interval must be shorter than --lease-ttl
--usage-stale-after must be longer than --lease-ttl
--body-idle-timeout must be longer than zero
--s3-request-timeout must be longer than zero
--bucket <name> is the name of a probe path
```

Fix the value; `journalctl -u tierstore` shows the message at every try. `--check-config` does not find these (see [configuration](configuration.md#the-flags-that-stay-flags)). In a file these name `hot_store.low_water`, `hot_store.high_water` and so on.

A key file:

```message
<flag> <path>: others can read it; make it mode 0600
<flag> <path>: is not owned by the user the server runs as
<flag> <path>: is a symbolic link; name the file itself
```

Run `chown tierstore` and `chmod 0600` on the file. The process refuses a file that its group can access too. For an unreadable or malformed file the message names the flag and the file and says what form it expects, never the content.

The same access key id on two keys: `the access key id "<id>" is used twice among --access-key-file and --read-only-key-file` (the flags it names are those of the keys that clash). Give each key an id of its own.

The configuration file:

```message
<key>: a secret is never in the file; give the path of the file that holds it (a key whose name ends in _file)
--config cannot be given with <flags>: the file holds what they hold (only --drain-lb-wait and --drain-deadline go with it)
<key>: a storage node keeps no cache: it needs [local_cache] and no [hot_store]
<key>: the file needs both [hot_store] and [cold_store] (the storage-node shape), or [local_cache] and [cold_store] without [hot_store] (the build-node shape)
<key>: is refused with [hot_store]: the hot store is already local disk, and a disk cache in front of it would cache a disk with a disk
<key>: is refused in the file of a build node: the OCI front is a storage node's, and this node serves only the S3 front
```

Move the setting into the file as a path, or take the flag out of the command line; give the file one of the two shapes, a storage node's or a build node's, and leave `[local_cache]` out of a storage node's. A message of a syntax error reads `line <N>, column <M>: <key>: ...` and never repeats the value.

The tiers:

```message
tier unreachable: <tier>: <error>
opening the proxy: <why>
binding <address>: <why>
```

`tier unreachable` is a store that did not answer for `--backend-check-wait`: check the endpoint, the network and the store. The process asks again with growing pauses before it gives up, so a store that is slow to come up after a reboot does not fail a start. `binding` is an address in use, or one that is not on this host.

A tier that answers a self-test question wrongly exits 3 and names the tier, the check and what it answered: the store overwrites on a create-only put, ignores `If-Match`, answers a range with the whole object, or completes a multipart upload over an existing object. Use a store that does these properly. `--skip-backend-check multipart-conditional=<reason>` is for the last only.

## Errors a client sees

| Status and code | Why |
|---|---|
| 403 `InvalidAccessKeyId` | the key id is unknown, or the request's region is not `--region` (the server's own, not a tier's) |
| 403 `SignatureDoesNotMatch` | the secret is wrong, or the request was changed after it was signed |
| 403 `RequestTimeTooSkewed` | the client's clock is more than 15 minutes from the server's |
| 403 `AccessDenied` | the key's role does not allow the call; see [keys and roles](keys.md) |
| 412 `PreconditionFailed` | the key already exists in either tier: puts are create-only |
| 413 `EntityTooLarge` | a body longer than `--max-request-bytes` (or the presigning key's ceiling) |
| 400 `RequestTimeout` | a body that stalled for `--body-idle-timeout` or came slower than `--put-min-bytes-per-sec`; nothing is stored |
| 503 `SlowDown` | more than `--max-concurrent-puts` bodies at once for longer than `--put-permit-wait`, or a put that met a full tier; retry |
| 503 `ServiceUnavailable` | the server is draining, or a tier could not be reached |
| 501 `NotImplemented` | an `aws-chunked` or trailing-checksum body, CRC-64/NVMe, or `encoding-type=url` on a listing; set the SDK's checksum calculation to "when required" |

## The hot tier keeps filling

Check `/usagez` first. A 503 there means the reconciler is not publishing: look at its process (`journalctl`), at the cold tier's reachability and at the lease. If usage is fresh and still high, the cold tier is taking copies slower than clients write: raise `--copy-parallel` if the cold store has room for more at once, or give the hot tier a larger `--hot-bytes`. See [operations](operations.md) for what a full hot tier does.

## At drain deadline

`tierstore: <N> requests were cut at the drain deadline` on stderr means clients were still sending or reading when `--drain-deadline` ran out. Raise it, with `TimeoutStopSec=` in the unit.
