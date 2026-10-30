# tierstore: an example unit

`tierstore` (`crates/tierstore`) is an S3 server over a hot and a cold tier of S3-compatible
stores: it writes new objects to the hot tier, copies them to the cold tier in the background, and
frees the hot tier when it fills. It is a release asset for Linux only, as a tarball with its
SBOM and its provenance bundle (`docs/artifacts.md`); there is no package and no container image.
These files go beside the binary.

| file | what |
|---|---|
| `tierstore.service` | the unit: the binary as `tierstore`, an account that is not root, a two-tier S3 setup, a stop that waits for the drain, and the hardening of the other example units |
| `tierstore.conf` | the sysusers.d entry of the `tierstore` account (no login, no home) |

The values in the unit are examples (`192.0.2.10`, `s3-hot.example`, the bucket names, the
sizes): edit them to the setup's. `crates/tierstore/tests/packaging.rs` reads these files and
checks that the command line parses as the binary parses it, that no secret is on it (every key
and credential flag names a file), that the stop timeout covers the drain, and that nothing
starts the unit by itself.

## Placing the binary

The release carries `tierstore-<commit>-linux-x86_64.tar.gz` and
`tierstore-<commit>-linux-arm64.tar.gz`, each with `<tarball>.sigstore.json` and listed in
`SHA256SUMS`. Take the tarball of the host's architecture by its digest, and verify it as every
tarball is verified (`docs/artifacts.md`, "Provenance bundles: verifying a tarball offline").
The tarball holds one member, the binary under its own name:

```sh
tar -xzf tierstore-<commit>-linux-x86_64.tar.gz tierstore
install -d -m 0755 /usr/libexec/kbf
install -m 0755 -o root -g root tierstore /usr/libexec/kbf/tierstore
```

Nothing updates it afterwards. The host updater (`kbf-updater`, `kbf-host-install`) does not
manage `tierstore` yet: whoever runs the setup places the next binary the same way and restarts
the unit.

## Placing the unit

```sh
install -m 0644 tierstore.conf /etc/sysusers.d/tierstore.conf
systemd-sysusers /etc/sysusers.d/tierstore.conf
install -d -m 0750 -o root -g tierstore /etc/tierstore
install -m 0644 tierstore.service /etc/systemd/system/tierstore.service
systemctl daemon-reload
```

Then write the four key files, each owned by `tierstore` with mode 0600 or 0400 (the process
refuses a key file that its group or others can read):

- `/etc/tierstore/access.key`: the full key, the one clients that write sign their requests to
  `tierstore` with (`--access-key-file`). It may do everything the server serves: put, get, head,
  list, delete, and the calls of a multipart upload;
- `/etc/tierstore/daemons.key`: a read-only key (`--read-only-key-file`), for the daemons that
  read blobs and must not write or delete them. A request signed with it may get and head an
  object, list the bucket (objects, and the open multipart uploads) and head the bucket; a put, a
  delete, CreateBucket or any call that starts, feeds, completes or aborts a multipart upload is
  answered 403 `AccessDenied`, before its body is read, and changes nothing.
  Give the flag once for each read-only key, to give each reader a key of its own. The access key
  ids of all the keys of one server must differ: a request names its key by its id, so a start
  with two keys that have one id is refused with exit status 2;
- `/etc/tierstore/hot.credentials` and `/etc/tierstore/cold.credentials`: the keys `tierstore`
  signs its requests to the two stores with (`--hot-credentials-file`,
  `--cold-credentials-file`). The two tiers may be two stores, and have a key each.

Every one of these files holds its key in one of two forms, and the process tells them apart by
the first byte of the file that is not white space: `{` is JSON, anything else is two lines.

```json
{"access_key_id": "AKIDEXAMPLE", "secret_access_key": "the secret"}
```

or, as two lines, the access key id and then the secret access key:

```text
AKIDEXAMPLE
the secret
```

The JSON form is the one that the pack flags of the frontend and of the daemon read
(`--pack-store-credentials-file`), but `tierstore` is stricter than they are: it needs exactly
the two fields, both non-empty strings, and refuses a file with another field, with a field
misspelled or with an empty value. So one file holds both the key that a daemon signs with and
the key that `tierstore` checks only if it has exactly those two non-empty fields; a pack-flags
file that has another field must be trimmed to be shared. `tierstore` takes the values of the
JSON form as they are, without trimming. No message of the process carries a secret.

A role belongs to a key, not to a prefix: a read-only key may read every object of the bucket.
A write limited to one object under a prefix is a presigned PUT URL (below).

## Presigned PUT URLs

A client that holds a presigned URL writes one object with a plain `PUT` of the URL: the server
checks SigV4 query-string authentication as the AWS documentation has it (`X-Amz-Algorithm`,
`X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, `X-Amz-Signature`;
payload `UNSIGNED-PAYLOAD`; `host` signed), so any SDK presigner or `tierstore::server::presign_put`
makes one. Only a presigning key (`--presign-key-file`, once per key, mode 0600) can sign a URL; the
full and read-only keys cannot, and a presigning key is refused for any request signed in the
header. The file holds the key and its ceiling, all required (a file without one is refused at
start):

```json
{"access_key_id": "pack-presign", "secret_access_key": "the secret",
 "ceiling": {"prefix": "cas/b/", "max_expires_secs": 600, "max_object_bytes": 134217728}}
```

A URL is good for one `PUT` of a key that starts with the prefix (after the path is decoded: no
`..`, no `.` segment, no control character, never `_journal/` or `_tiering/`), with an
`X-Amz-Expires` of at most the smaller of `max_expires_secs` and `--presign-max-expires` (default
15m), and a body of at most `max_object_bytes` (5 GiB at most, the S3 single-PUT limit; each key file chooses its own), checked on
`Content-Length` and on the bytes received (and held to `--max-request-bytes`, 256 MiB by default, too: a key with a larger
cap needs that raised, and the smaller of the two applies; the body is streamed to the tier and
never held whole). It is refused from `X-Amz-Date` plus `X-Amz-Expires` on, with no grace. The
object appears only after a complete body, a second `PUT` of the key is `412`, and no multipart
call, `If-Match`, read, list or delete is allowed. The signature does not cover the body, so the
reader of the object must check its content. The query string is never logged.

A secret is never on the command line or in the environment: the process reads no environment
variable, and the unit sets none.

## GC keys

A GC key (`--gc-key-file`, once per key; JSON of `access_key_id`, `secret_access_key` and `prefix`,
mode 0600, not a symbolic link, owned by the user the server runs as) is for the process that
collects the objects that presigned PUTs wrote. It signs in the header only (a presigned URL signed
with it is refused) and may do these, only for a key that starts with its prefix (which ends in `/`)
and is longer than it:

| call | allowed |
|---|---|
| ListObjectsV2 | with a `prefix` inside the key's; a list with no prefix or another is 403, not filtered |
| ListMultipartUploads | with a `prefix` inside the key's, the open uploads under it; no prefix or another is 403, not filtered |
| HeadObject | yes |
| DeleteObject | yes: from both tiers, journaled as for any delete |
| AbortMultipartUpload | of an upload of such a key |
| GetObject | no: a GC key never reads a body |
| put, part, start or completion of an upload, copy, CreateBucket, HeadBucket | no |
| `_journal/`, `_tiering/`, any key outside the prefix, the prefix itself | no |

A call the key may not do is refused with 403 `AccessDenied` without its body being read. The
refusals are made in this order: the signature and key id, the method, the decoding of the path and
query (400, or 404 for an unknown bucket), the checksum headers (400 or 501), and last the key or
list prefix; so a call that is both refused and malformed may get the malformed-request error. A delete by a GC key is not
create-only; it is the only way an object leaves the prefix that presigned PUTs write.

## Filling in the flags

The unit's `ExecStart=` is a complete command line. The flags to edit:

| flag | what |
|---|---|
| `--listen` | the address the S3 API, `/healthz` and `/readyz` are served on |
| `--proxy-id` | names this process's journal and its lease in the hot tier; keep it across restarts, and give each process over the same tiers one of its own |
| `--bucket` | the name of the one bucket clients see |
| `--access-key-file`, `--read-only-key-file` | the full key, and (given once for each) the read-only keys; see above |
| `--presign-key-file`, `--presign-max-expires` | a presigning key with its ceiling (given once for each key), and the longest expiry of any URL; see Presigned PUT URLs |
| `--gc-key-file` | a GC key (given once for each key); see GC keys |
| `--hot`, `--cold` | the tiers: `s3:<bucket>` for a bucket of an S3-compatible store (also `dir:<path>` and `memory`; a `dir:` path must also be writable under `ProtectSystem=strict`, so add it to `ReadWritePaths=`) |
| `--hot-endpoint`, `--cold-endpoint` | where the tier's store is, `http://host[:port]` (TLS is not built yet); `--hot-region` and `--cold-region` set the signing region of a tier, `us-east-1` if not given |
| `--hot-bytes` | the bytes the hot tier is meant to hold; the water marks are shares of it (`--high-water`, `--low-water`, `--critical`) |
| `--drain-lb-wait`, `--drain-deadline` | the drain on SIGTERM: how long `/readyz` says 503 while the server goes on serving, and how long it then waits for requests in flight |
| `--usage-stale-after` | how old the usage that the reconciler publishes may be (2m by default, longer than `--lease-ttl`) before `/usagez` says 503 with the reason (200 and the age while fresh). Puts ignore a usage older than 30 s, so a stopped reconciler means a hot tier that is never taken to be full; puts are not refused. A load balancer probes `/readyz`, which does not look at the usage (one stopped reconciler would otherwise take every node out); alerting probes `/usagez` |
| `--oci-listen`, `--oci-tls-cert`, `--oci-tls-key`, `--oci-prefix` | the read-only registry front, off unless `--oci-listen` is given; see The OCI read front |

`tierstore --help` lists them all, with their defaults.

## Using a stock SDK

The front serves one bucket, path-style, over plain HTTP, and checks SigV4 in the `Authorization`
header only. An SDK with its defaults sends some things the front refuses, so a client needs these
settings. `crates/tierstore/tests/s3_front/sdk.rs` drives the front with aws-sdk-rust (the
`aws-sdk-s3` crate, version 1.96.0, in this repository's tests only) set this way, and that test is
what "tested" means below.

| setting | value | why |
|---|---|---|
| endpoint | `http://<host>:<port>` of `--listen` | the front has no TLS; an `https://` endpoint fails |
| addressing | path-style (`force_path_style`) | the bucket is in the path, `/<bucket>/<key>`; virtual-host addressing (`<bucket>.<host>`) is not served |
| bucket | the `--bucket` name | there is one; other names are not served |
| region | tierstore's own `--region` (not `--hot-region` or `--cold-region`, which are for the tiers' stores), `us-east-1` by default | the region is in the credential scope of the signature, and a client with another is refused 403 `InvalidAccessKeyId` |
| credentials | a static key id and secret: the full key, or a read-only key | the front has no session tokens, no STS and no presigned URLs |
| request checksums | only when an operation requires one (`request_checksum_calculation = when_required`) | the SDK default sends a checksum with every upload, and for a streamed body sends it as a trailer in an `aws-chunked` body, which the front refuses with 501 |
| response checksums | only when required (`response_checksum_validation = when_required`) | the front sends no checksum on a download, so there is nothing to validate; this keeps the SDK from asking |
| checksum algorithm | none, or CRC-32, CRC-32C, SHA-1 or SHA-256 | read from the front's code, not tested with an SDK: CRC-64/NVMe, `aws-chunked` and trailing checksums are refused with 501; `Content-MD5` is checked |
| `x-id` | nothing to set | the AWS SDKs add `x-id=<Operation>` to the query of the calls of an object, and no setting stops it; S3 ignores it, and so does the front |
| listing | no `encoding-type` | `encoding-type=url` is refused with 501 (aws-sdk-rust sends it only when asked to) |

Payload signing, read from the front's code and not tested as such (not checked: whether the Rust
SDK signs the hex hash or `UNSIGNED-PAYLOAD` for an in-memory body): a body whose signed `x-amz-content-sha256` is its hex SHA-256 is checked against it;
`UNSIGNED-PAYLOAD` with no checksum header is accepted unchecked; a `STREAMING-` payload is
refused with 501.

aws-sdk-rust, as the test sets it (the test also passes the SDK's HTTP client built without TLS,
`aws_smithy_http_client::Builder::new().build_http()`, which the lines below leave out):

```rust
use aws_sdk_s3::config::{Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation};

let config = aws_sdk_s3::Config::builder()
    .behavior_version_latest()
    .endpoint_url("http://tierstore.example:9000")
    .force_path_style(true)
    .region(Region::new("us-east-1"))
    .credentials_provider(Credentials::new(KEY_ID, SECRET, None, None, "static"))
    .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
    .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
    .build();
let client = aws_sdk_s3::Client::from_conf(config);
```

With these, the test puts an object, gets it whole and by byte ranges, heads it, lists with a prefix
and a continuation token, deletes it, uploads in parts (create, two parts, complete) and aborts an
upload. A second put of a key, with `If-None-Match: *` or without, comes back as an error with
status 412 and code `PreconditionFailed`. A read-only key gets, heads and lists, and its put and
delete come back as 403 `AccessDenied`.
The aws CLI and boto3 are **not tested**: the lines below are from their documentation, and
whether they are enough is not shown. They need a version that has the checksum settings (aws CLI
2.23 or later, boto3 1.36 or later).

```ini
# ~/.aws/config
[default]
region = us-east-1
endpoint_url = http://tierstore.example:9000
request_checksum_calculation = when_required
response_checksum_validation = when_required
s3 =
    addressing_style = path
```

```python
# boto3
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://tierstore.example:9000",
    region_name="us-east-1",
    aws_access_key_id=KEY_ID,
    aws_secret_access_key=SECRET,
    config=Config(
        signature_version="s3v4",
        s3={"addressing_style": "path"},
        request_checksum_calculation="when_required",
        response_checksum_validation="when_required",
    ),
)
```

One thing is known from the front and not from those clients: boto3 and the aws CLI are said to
add `encoding-type=url` to a ListObjects or ListObjectsV2 call on their own, with no setting to
stop it, and the front answers that parameter with 501 `NotImplemented`. If that is so, `aws s3
ls` and `list_objects_v2` fail against the front, and the calls that name a key (`get_object`,
`put_object`, `head_object`, `delete_object`, multipart) are not affected. Check it with a real
client before relying on either.

## The OCI read front

With `--oci-listen`, the process also serves the objects of the store as a read-only container
registry, over HTTPS or over plain HTTP on loopback (see Two modes), so that podman (and the
other clients of the OCI distribution API) pulls an image by digest from the same hot and cold tiers the S3 front writes to. It reads through the
proxy in the process: no S3 credential and no hop through the S3 listener. It is off unless
`--oci-listen` is given, and then nothing else about the process changes.

| flag | what |
|---|---|
| `--oci-listen ADDR` | where the registry front listens. With `--oci-tls-cert` and `--oci-tls-key` it is HTTPS; with neither it is plain HTTP and ADDR must be a loopback address (127/8 or `::1`): anything else, `0.0.0.0` and `::` included, is refused at start (exit 2) |
| `--oci-tls-cert PATH`, `--oci-tls-key PATH` | both or neither (one without the other is refused at start, exit 2). The certificate chain (leaf first) and the private key, PEM. Clients dial an IP address, so the leaf must carry an IP subjectAltName for `--oci-listen` (any IP subjectAltName, if the address is `0.0.0.0` or `::`); a certificate without one, one that is expired or not yet valid, a key file any user can read (the "other" mode bits; a group may), or an unreadable file, is refused at start. The files are read once: a new certificate takes a restart, so renew it before it expires |
| `--oci-prefix PREFIX` | the key prefix, ending in `/`, `oci/` if not given. A prefix inside `_journal/` or `_tiering/`, or one that is not plain path segments, is refused at start (exit 2) |
| `--oci-max-connections N` | connections open at once, 256 if not given; the others wait in the kernel's backlog |
| `--oci-connection-lifetime D` | the longest a connection lives, 1h if not given; a request being answered is finished first |

A client has 10 seconds to finish the TLS handshake and then the S3 front's header timeout (a
minute) to send the head of a request, which is also how long an idle connection may wait for its
next request. A connection holds one of the `--oci-max-connections` slots from accept to close and
there is no limit per client address, so a host that opens that many connections and says nothing
can keep the others waiting: the host's firewall rules are the control for that. A client that takes no byte of an answer for `--body-idle-timeout`
is cut. A connection that reaches its lifetime finishes the answer it is sending for at most
`--drain-deadline`, and is cut then. On the drain the front goes on serving through `--drain-lb-wait`, takes no new connection,
lets the requests being answered finish, and closes what is left at `--drain-deadline`. The start
line on stdout gets ` oci-listen=<address> (https)` or ` (http)` after `listen=<address>`, and `/metricz`
has `tierstore_oci_front_info{mode="https"}` or `{mode="http"}`. In plain mode there is no handshake,
so the handshake time does not apply; the other limits are the same.

### Two modes

- **HTTPS**: with `--oci-tls-cert` and `--oci-tls-key`, at any address.
- **Plain HTTP**: with neither, on loopback only. A node's own container engine can pull from the
  front of its own tierstore, so the pull is a connection from the host to itself and the traffic
  never leaves it. A connection to any address beyond loopback can be read and changed on
  its way, so it needs TLS. Loopback is reachable by every local process, and by containers that
  share the host's network namespace; the front is anonymous in either mode.

A container engine that pulls over plain HTTP has to be told it may:

- podman: mark the registry insecure for that location in `registries.conf`, e.g.

  ```toml
  [[registry]]
  location = "127.0.0.1:9500"
  insecure = true
  ```

  or pull with `--tls-verify=false`. No `certs.d` directory is needed.
- js-oci needs to be allowed plain HTTP to a loopback registry address for such a pull; that is a
  setting of js-oci, not of tierstore.

What it serves, and nothing else (any other path is 404 and any method but GET and HEAD is 405
`UNSUPPORTED` with `Allow: GET, HEAD`, both with the OCI error document):

| request | answer |
|---|---|
| `/v2/` | 200, `{}`, `Docker-Distribution-API-Version: registry/2.0` |
| `/v2/<name>/manifests/sha256:<hex>` | the manifest (at most 4 MiB), with `Content-Type` from its `mediaType`, `Content-Length`, `Docker-Content-Digest` and an `ETag` of the digest. A tag is 404 `MANIFEST_UNKNOWN`: tags are not served |
| `/v2/<name>/blobs/sha256:<hex>` | the blob, streamed, `application/octet-stream`, with `Accept-Ranges: bytes`; one `Range` is 206 with `Content-Range`, one past the end 416 |

The `Content-Type` of a manifest is its `mediaType` when that is the OCI image manifest or index or
the Docker schema 2 manifest or manifest list. A manifest with none, or another, is served as an OCI
image manifest if it is `schemaVersion` 2 with a `config` object and a `layers` array, as an OCI
image index if it is `schemaVersion` 2 with a `manifests` array (an index may leave `mediaType`
out), and is 500 `MANIFEST_INVALID` with the reason otherwise.

A malformed digest (not `sha256:` and 64 lowercase hex digits) is 400 `DIGEST_INVALID`, a repository
name that is not valid by the distribution grammar is 400 `NAME_INVALID`, an object that is not in
the store is 404 `BLOB_UNKNOWN` or `MANIFEST_UNKNOWN`, a store that cannot be reached is 503, and
what the store said stays in the log.

### Keys, and one namespace

An object is at `<prefix>blobs/sha256/<64 lowercase hex digits>`, the same for a manifest, an index,
a layer and a config: manifests are blobs. The `<name>` in a URL is checked against the grammar and
then ignored, so there is one content-addressed namespace and any name pulls any digest. A key is
built only from a validated digest, so nothing outside the prefix can be named.

### Integrity

A manifest is read whole and its SHA-256 is checked against its digest before a byte is sent; one
that does not match is 500 `DIGEST_INVALID`, is never served and is counted. A blob is hashed as it is
read, in chunks of 4 MiB, and its last chunk is held back until the hash is right: a blob that does
not match its digest ends the connection before the response is complete, so no client receives a
whole wrong body, and the mismatch is counted. That holds for a read of the whole blob, and for a
`Range` that covers all of it (which is still 206). A blob `HEAD` and a `Range` of part of a blob
report what the store holds and verify nothing; the client verifies the digest of what it
assembles. (A manifest `HEAD` reads and verifies the manifest.) A client that sends several ranges
(as a partial pull of a `zstd:chunked` or `estargz` layer does) is sent the whole blob with 200,
which the specification allows, and falls back to splitting it. `If-Range`, `If-None-Match` and
`Accept` are ignored.

### Anonymous reads

The front never asks for a token: a request with an `Authorization` header is served as one without.
Anyone who can connect can read any digest under the prefix, and nothing else. That is the intent:
the images in it are public (a public registry's images are the first thing put there), and the
network decides who can connect (the host's firewall rules). Do not put an image in
the prefix that is not meant to be read by whoever can reach the port.

### Putting an image in the store

Take the blobs of an OCI layout (the files under `blobs/sha256/` of a layout made with
`skopeo copy ... oci:<dir>` or `podman save --format oci-dir`) and put each, create-only, at
`<prefix>blobs/sha256/<hex>` through the S3 front with the full key: the layers, the config, the
manifest, and the index of a multi-platform image, all by the file's own name, which is its digest.
`index.json` of the layout is not put; the digest of the manifest (or index) to pull is in it, and
is what a client is given. A layer larger than `--max-request-bytes` (256 MiB if not given) needs the
flag raised, or a multipart upload, or a presigned PUT whose key has a `max_object_bytes` of up to
5 GiB (see Presigned PUT URLs; the ceiling prefix must cover `oci/`). An object that is already
there is refused (412), which is a retry that is safe.

Check a put with `HEAD` on the front: the `Docker-Content-Digest` of a manifest is the digest of the
bytes it read, after they were checked.

### Pulling

Over plain HTTP on loopback there is no CA to trust; see Two modes for the client settings.

podman, from a host that trusts the CA of the certificate:

```sh
# the CA that issued --oci-tls-cert, in the directory named like the registry in the reference
# (the address, with :port when the reference has one)
install -D -m 0644 farm-ca.crt /etc/containers/certs.d/192.0.2.10/ca.crt
podman pull 192.0.2.10/library/app@sha256:<manifest digest>
```

The reference is by digest: a tag is refused. The repository name (`library/app`) is any name that
fits the grammar and means nothing to the store. A registry on another port than 443 is written
`192.0.2.10:8443/library/app@sha256:...` and its directory is `certs.d/192.0.2.10:8443`.

A client that allows only `https` registries on port 443 (or the port in the reference), and
registries at a private address only when told to, pulls the same references; allow the address in
that client's own settings.

### Counters

`GET /metricz` on the S3 listener (unsigned, like `/healthz`; a bucket cannot be called `metricz`)
shows, in the Prometheus text format, `tierstore_oci_requests_total{route,outcome}` (routes `ping`,
`manifest`, `blob`, `other`; outcomes `ok`, `partial`, `not_found`, `invalid`, `not_allowed`,
`range_not_satisfiable`, `failed`), `tierstore_oci_bytes_served_total` (bytes of blobs read for
responses), `tierstore_oci_digest_mismatches_total` and `tierstore_oci_front_info{mode="https"|"http"}` (1; the mode the front runs in). They are not on the registry listener,
which serves nothing else. Without `--oci-listen` the path says the front is not enabled.

## Starting it

The unit does not start at install and does not start at boot: it has no `[Install]` section,
so `systemctl enable` has nothing to link, and it is skipped while `/etc/tierstore/access.key`
does not exist. Start it by hand when the files are in place:

```sh
systemctl start tierstore
```

To start it at boot, add the section in a drop-in (`systemctl edit tierstore` takes
`[Install]` and `WantedBy=multi-user.target`) and run `systemctl enable tierstore`; that is
the operator's choice, and the shipped file does not make it.

## Stopping it

systemd sends SIGTERM. The process answers `/readyz` 503, serves for `--drain-lb-wait` so that a
load balancer sees it, refuses new requests, waits for those in flight for at most
`--drain-deadline`, gives up the reconciler's lease and exits 0. `TimeoutStopSec=` is set above
the sum of the two flags and the lease's wait, so that systemd does not send SIGKILL to a drain
in progress; if `--drain-deadline` is raised, raise it too. A start that fails is tried again
after 30 seconds; exit status 2 (a command line the argument parser refused: an unknown flag, a
missing or malformed value; or two keys with one access key id) is not, since a restart cannot
mend it. Other flag combinations and key files refused after parsing exit 1, and are tried again
every 30 seconds.

## The start-up self-test

Before it binds its address the process asks each tier the questions its guarantees rest on: a
create-only put of a key that exists must be refused, a replace on a wrong ETag must be refused
and one on the right ETag must work, a ranged read must give exactly the range, a HEAD must give
the size and the ETag of what was written, a listing after a key must page right, and on the cold
tier the completion of a multipart upload over a key that exists must be refused. (Only the
create-only completion is probed, not the refusal of a bad part list.) It uses keys under
`_tiering/selftest/`, which clients never see; they are deleted afterwards, and keys a killed
start left more than an hour ago (by the store's clock, not the machine's) are swept; only keys
under that prefix are ever deleted. A tier that answers one of them wrongly exits 3
with the tier, the check and the answer on stderr, and is not restarted
(`RestartPreventExitStatus=2 3`): fix or replace the store. That includes answers that are a
configuration error, such as 403 `AccessDenied` or `NoSuchBucket`, which are not retried either.
A tier that cannot be reached (down, a timeout, 408, 429, any 5xx but 501 and 507) is asked again,
after pauses from one second doubling to thirty, for `--backend-check-wait` (five minutes by
default, for each tier; `0s` asks once), so that a store that is slow to come up after a reboot
does not fail the start. Then the start exits 1 and the unit tries it again after 30 seconds, up to
its start limit (5 starts in an hour, so about half an hour of real failures, then systemd gives up
until `systemctl reset-failed tierstore`). An
error that persists on a conditional step (a create-only put, `If-Match`, the completion of a
multipart upload) while the store is up, a 500 for instance, may be a gap in the store and not an
outage; the message says so.
A tier that is full (a full hot tier is a designed state) starts: the checks that need a write are
skipped, with a line on stderr that says so. For a full cold tier the line is a warning that writes
to the cold tier will fail.

`--skip-backend-check multipart-conditional=<reason>` leaves out the multipart check, for a store
that lacks a conditional completion; the reason is written to stderr at every start. No other check
can be skipped (exit 2). Without that check a multipart upload to the cold tier may replace an
object that exists.
