-- kbf-migration: expand
-- Directory rows: the bytes of each REAPI Directory message the frontend has walked, by the
-- Directory's own digest, in the state store beside the pack index. The table is new, and like
-- the pack index's it lives outside kv, feed and the commit head, so a write takes none of the
-- locks a state transaction queues on. Names are unqualified; the runner sets the search path
-- to the store's schema.
--
-- directory: one row per Directory digest (`hash` and `size`, the digest of `body`). `body` is
-- the encoded Directory exactly as the client uploaded it, so its SHA-256 is `hash` and its
-- length is `size`; the index checks both before it inserts. A row is content-addressed and
-- never changes: a second write of the same digest is the same bytes and changes nothing.
-- `written_us` is the store's clock at the insert. The Directory's blob keeps its own row in
-- `blob` (it is still a blob of a pack); this row lets a walk of an input tree read a level of
-- Directories with one query instead of one ranged read of the object store each.
CREATE TABLE directory (
    hash bytea NOT NULL,
    size bigint NOT NULL,
    body bytea NOT NULL,
    written_us bigint NOT NULL,
    PRIMARY KEY (hash, size));
