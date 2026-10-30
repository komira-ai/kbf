# tierstore

A generic hot/cold object store over S3-compatible backends. It knows nothing of kbf: its normal
dependencies include no other crate of this workspace unless the `s3` feature is on
(`crates/kbf-integration-test/tests/layering.rs` keeps it that way), so it can move to a repository
of its own.

- The library: the `Backend` trait (create-only put, `If-Match` put, ranged get, head, delete, list,
  multipart), an in-memory and a directory backend, `Proxy` (a journal per proxy, a get that copies a
  cold hit back, a full hot tier sending puts to the cold tier), `Reconciler` (copies puts to the
  cold tier, applies deletes, evicts what the cold tier holds, publishes usage) and `CachedStore`.
- Feature `s3`: `S3Backend`, a bucket of an S3-compatible store as a `Backend`.
- Feature `server` (turns `s3` on): `server`, the S3 API front of a proxy, and the `tierstore`
  binary that runs a proxy, that front and a reconciler in one process.
  `packaging/tierstore/README.md` has the flags, the keys and the unit.

## The OCI read front

With `--oci-listen ADDR` the binary also serves the objects of the store as a read-only container
registry (`server::oci`), so that podman and other registry clients pull an image by digest from the
same tiers. It runs in one of two modes:

- **HTTPS**, with `--oci-tls-cert PATH --oci-tls-key PATH`, at any address.
- **Plain HTTP**, with neither flag, and only on a loopback address (127.0.0.0/8 or `::1`). A node's
  own container engine pulls from the front of its own store over loopback, so the traffic never
  leaves the host. Any other address, `0.0.0.0` and `::` included, is refused at start (exit 2) and
  needs the certificate and key. One of the two flags without the other is refused too.

The start line and `/metricz` say which mode runs.

- `GET` and `HEAD` of `/v2/`, `/v2/<name>/manifests/sha256:<hex>` and
  `/v2/<name>/blobs/sha256:<hex>`; anything else is 404, and any other method 405.
- An object is at `<--oci-prefix>blobs/sha256/<hex>` (`oci/` by default), manifests included. The
  repository name in a URL is checked against the distribution grammar and then ignored: one
  content-addressed namespace.
- Anonymous: no token is asked for and `Authorization` is ignored. Only digest-named objects under
  the prefix can be read.
- A manifest is checked against its digest before it is sent; a blob is hashed as it is read and cut
  short, not delivered whole, if it is not its digest. Tags are not served.
- In HTTPS mode the certificate must carry an IP subjectAltName for the listen address (clients dial
  an IP), checked at start. The files are read once: a new certificate takes a restart. The
  connection limits, timeouts and drain are the same in both modes (a plain connection has no
  handshake to time).
- `GET /metricz` on the S3 listener has the counters.

Putting an image in, and pulling it with podman and js-oci, are in `packaging/tierstore/README.md`
("The OCI read front"); the behaviour is specified by the tests in `tests/oci_front` (over real TLS and over plain HTTP, both
on loopback) and `src/server/oci`.
