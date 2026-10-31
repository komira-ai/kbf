# Keys and roles

A client signs each request with an access key (SigV4, in the `Authorization` header). The key it signs with decides what the request may do. The role belongs to the key, not to the client or the address.

## The keys, and what each may do

| Key | Flag | Key of the file | May do |
|---|---|---|---|
| full | `--access-key-file` | `server.access_key_file` | everything the server serves: put, get, head, list, delete and multipart uploads |
| read-only | `--read-only-key-file` | `server.read_only_key_files` | get and head objects, list the bucket and its open uploads, head the bucket. Any write, delete, CreateBucket or multipart call is 403 `AccessDenied`, before its body is read |
| presigning | `--presign-key-file` | `server.presign_key_files` | sign presigned PUT URLs, each good for one object under the key's prefix, and nothing else |
| GC | `--gc-key-file` | `server.gc_key_files` | list, head, delete and abort uploads under one prefix, and nothing else; it never reads a body |
| scoped | `--scoped-key-file` | `server.scoped_key_files` | read, create and delete under the prefixes its file lists, and nothing else |
| grant | `--grant-key-file` | `server.grant_key_files` | signs no request: its signature on a read grant (the `x-tierstore-grant` header) lets the bearer get and head the objects the grant lists, until the grant expires, and nothing else |
| hot tier credential | `--hot-credentials-file` | `hot_store.credentials_file` | not a client key: what tierstore signs its own requests to an `s3` hot tier with |
| cold tier credential | `--cold-credentials-file` | `cold_store.credentials_file` | not a client key: the same for an `s3` cold tier |

Only the full key is required. The read-only, presigning, GC, scoped and grant flags may each be given once for each key. A tier that is a directory needs no credential file.

A role is not a prefix: a read-only key may read every object of the bucket. To limit a client to prefixes, use a scoped key (or a GC key, or a presigned URL). The key prefixes `_journal/` and `_tiering/` belong to tierstore: a client put or delete there is refused, and a listing leaves them out.

## The key file

Every key file holds its key in one of two forms, told apart by the first byte that is not white space: `{` is JSON, anything else is two lines.

```json
{"access_key_id": "AKIDEXAMPLE", "secret_access_key": "the secret"}
```

or, as two lines, the access key id and then the secret access key:

```text
AKIDEXAMPLE
the secret
```

The JSON form needs exactly the two fields, both non-empty; a file with another field, a misspelled field or an empty value is refused. Values are taken as they are, without trimming. Presigning, GC and scoped key files are JSON with more fields (below).

## File permissions

Each key file must be owned by the user the server runs as, with mode 0600 or 0400. The process refuses a file that its group or others can access, and a symbolic link (for any key file). No message of the process carries a secret. The server reads the environment for nothing, and a secret is never on the command line or in the configuration file: the file holds only the paths of key files.

The access key ids of all the keys of one server must differ, because a request names its key by its id. A start with two keys that share an id is refused with exit status 2.

## Presigning keys

A presigning key file holds the key and its ceiling, all required:

```json
{"access_key_id": "pack-presign", "secret_access_key": "the secret",
 "ceiling": {"prefix": "cas/b/", "max_expires_secs": 600, "max_object_bytes": 134217728}}
```

A presigned URL is good for one PUT of a key under the prefix, for at most the smaller of `max_expires_secs` and `--presign-max-expires` (15 minutes by default), with a body of at most `max_object_bytes` (up to 5 GiB). The body is also held to `--max-request-bytes`; the smaller of the two applies. A second PUT of the key is 412. The signature does not cover the body, so whoever reads the object must check its content.

## GC keys

A GC key file is JSON of exactly `access_key_id`, `secret_access_key` and `prefix`, where the prefix ends in `/`. The key signs in the header only. It may list (ListObjectsV2 and ListMultipartUploads, each with a `prefix` inside its own), head, delete, and abort uploads, for keys longer than its prefix. Delete removes the object from both tiers. A list with no prefix or another is 403, not filtered.

## Scoped keys

A scoped key file is JSON of exactly five fields, all required:

```json
{"access_key_id": "frontend-packs", "secret_access_key": "the secret",
 "read_prefixes": ["cas/"],
 "write_prefixes": ["cas/f/frontend/"],
 "delete_prefixes": []}
```

Every prefix ends in `/` and none may name or lie under `_journal/` or `_tiering/`. A key whose three lists are all empty is refused. The lists are not interchangeable: a write prefix does not let the key read what it wrote, and a read prefix does not let it write. A put by a scoped key is create-only (no `If-Match`, no copy), and a delete needs a delete prefix.

## Rotation

The server reads its key files once, at start. To rotate a key, write the new file, then restart the unit: it drains first (see [operations](operations.md)). There is no reload.

A change to the tier credentials takes the same restart.

## Read grants

A client that holds no key and must read a fixed set of objects for a short time presents a read grant: a token signed with a grant key, sent as one `x-tierstore-grant` header on `GET` and `HEAD`. It names one bucket, an expiry and the exact objects it lists, and allows nothing else. The server refuses a grant that ends more than `--grant-max-lifetime` (15 minutes by default) after its own clock. A grant is a bearer token: whoever holds it can use it until it expires, so use TLS where that matters. The token format is in the README, under Read grants.
