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
  object, list the bucket and head the bucket; a put, a delete, CreateBucket or any call of a
  multipart upload is answered 403 `AccessDenied`, before its body is read, and changes nothing.
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

A role belongs to a key, not to a prefix: a read-only key may read every object of the bucket,
and there is no key yet that may write under one prefix only.

A secret is never on the command line or in the environment: the process reads no environment
variable, and the unit sets none.

## Filling in the flags

The unit's `ExecStart=` is a complete command line. The flags to edit:

| flag | what |
|---|---|
| `--listen` | the address the S3 API, `/healthz` and `/readyz` are served on |
| `--proxy-id` | names this process's journal and its lease in the hot tier; keep it across restarts, and give each process over the same tiers one of its own |
| `--bucket` | the name of the one bucket clients see |
| `--access-key-file`, `--read-only-key-file` | the full key, and (given once for each) the read-only keys; see above |
| `--hot`, `--cold` | the tiers: `s3:<bucket>` for a bucket of an S3-compatible store (also `dir:<path>` and `memory`; a `dir:` path must also be writable under `ProtectSystem=strict`, so add it to `ReadWritePaths=`) |
| `--hot-endpoint`, `--cold-endpoint` | where the tier's store is, `http://host[:port]` (TLS is not built yet); `--hot-region` and `--cold-region` set the signing region of a tier, `us-east-1` if not given |
| `--hot-bytes` | the bytes the hot tier is meant to hold; the water marks are shares of it (`--high-water`, `--low-water`, `--critical`) |
| `--drain-lb-wait`, `--drain-deadline` | the drain on SIGTERM: how long `/readyz` says 503 while the server goes on serving, and how long it then waits for requests in flight |

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
| region | the `--region` of the server, `us-east-1` by default | the region is in the credential scope of the signature, and a client with another is refused 403 `InvalidAccessKeyId` |
| credentials | a static key id and secret: the full key, or a read-only key | the front has no session tokens, no STS and no presigned URLs |
| request checksums | only when an operation requires one (`request_checksum_calculation = when_required`) | the SDK default sends a checksum with every upload, and for a streamed body sends it as a trailer in an `aws-chunked` body, which the front refuses with 501 |
| response checksums | only when required (`response_checksum_validation = when_required`) | the front sends no checksum on a download, so there is nothing to validate; this keeps the SDK from asking |
| checksum algorithm | none, or CRC-32, CRC-32C, SHA-1 or SHA-256 | CRC-64/NVMe, `aws-chunked` and trailing checksums are refused with 501; `Content-MD5` is checked |
| `x-id` | nothing to set | the AWS SDKs add `x-id=<Operation>` to the query of the calls of an object, and no setting stops it; S3 ignores it, and so does the front |
| listing | no `encoding-type` | `encoding-type=url` is refused with 501 (aws-sdk-rust sends it only when asked to) |

aws-sdk-rust, tested:

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
delete come back as 403 `AccessDenied`. (The test builds the SDK's own HTTP client without TLS; an
application that uses the SDK's default HTTP client needs no setting for that, since the endpoint
says `http://`.)

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
after five seconds; exit status 2 (a command line the argument parser refused: an unknown flag, a
missing or malformed value; or two keys with one access key id) is not, since a restart cannot
mend it. Other flag combinations and key files refused after parsing exit 1, and are tried again
every five seconds.
