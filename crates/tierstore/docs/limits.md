# Limits

What tierstore does not do on main, so that you can decide before you depend on it.

- **No TLS on the S3 listener.** It serves plain HTTP, and an S3 tier's endpoint is `http://`. Put it on a network you trust, or terminate TLS in front of it. (The OCI front has TLS, `--oci-tls-cert` and `--oci-tls-key`.)
- **One bucket, path-style, SigV4 in the header.** No virtual-host addressing, no session tokens, no STS. Presigned PUT URLs are the one exception to signing in the header.
- **Objects are immutable.** A put is create-only; there is no overwrite and no copy. A delete removes both tiers' copies.
- **Bodies are streamed, with limits.** A put or part is passed on as it arrives and never held whole, but it is bounded by `--max-request-bytes` (256 MiB by default); a larger object needs the flag raised, a multipart upload or a presigned PUT. At most `--max-concurrent-puts` bodies are taken at once. A manifest served by the OCI front is read whole (at most 4 MiB).
- **No scrub of stored objects.** Nothing re-reads a storage node's objects to find bit rot. A cache node in a release does not hash a hit again either: an object that rots in its directory is served until it is evicted (the scrub below is not in a release yet). The OCI front checks a blob's SHA-256 as it serves it; the S3 front checks the checksums a client sends when it writes.
- **Copies are not atomic with deletes at the edges.** Two puts of one key at the same moment, one to each tier, may both succeed when the hot tier has just filled; and a put dropped by its caller after the hot tier took it and before its journal entry was written leaves an object that is never copied. See the proxy's source notes for these.
- **A hot tier that is lost loses what was not yet copied.** A put is acknowledged when its journal entry is durable in the hot tier, not when the cold tier has a copy. The reconciler publishes the copy lag in the hot tier; `/metricz` does not export it.
- **Checksums.** CRC-32, CRC-32C, SHA-1, SHA-256 and `Content-MD5` are checked; CRC-64/NVMe, `aws-chunked` bodies and trailing checksums are refused with 501 `NotImplemented`. A client SDK needs its checksum settings turned to "when required".
- **Anonymous OCI reads.** The registry front never asks for a token. Put only public images under its prefix.
- **No update mechanism.** The host updater does not manage tierstore yet; place the next binary and restart.
- **Linux only,** as a release tarball for x86_64 and arm64.

## Not in a release yet

- A background scrub of a build-node cache: it re-reads each cached object at `local_cache.scrub_rate` and drops one whose bytes no longer hash to the digest the cache recorded, so the next read fills it again. A storage node's objects are not scrubbed.
