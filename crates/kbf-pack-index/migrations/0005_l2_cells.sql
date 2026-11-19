-- kbf-migration: expand
-- The L2 sidecar's inputs: the SHA-256 of each 1 MiB cell of an object, and the tree lists
-- whose sidecar is not written yet, in the state store beside the pack index. Both tables are
-- new, and like the pack index's they live outside kv, feed and the commit head, so a write
-- takes none of the locks a state transaction queues on. Names are unqualified; the runner sets
-- the search path to the store's schema.
--
-- object_cells: one row per object key (a pack's key or a whole object's). `size` is the
-- object's length in bytes, at least 1, and `digests` is the SHA-256 of each 1 MiB cell of its
-- bytes, 32 bytes each, concatenated in order (the last cell short), so it is exactly
-- ceil(size / 1 MiB) * 32 bytes long; the index checks that before it inserts. A key names one
-- sequence of bytes, so a row never changes: the same cells again change nothing, and other
-- cells under the key are refused.
--
-- unlisted_tree: one row per tree list (`tree`, the 32-byte SHA-256 of its version 2 list)
-- whose sidecar could not be written because an object it names has no cells yet. `first_us`
-- and `last_us` are the store's clock when it was first and last recorded; a row is removed
-- once its sidecar is written.
CREATE TABLE object_cells (
    key text PRIMARY KEY,
    size bigint NOT NULL,
    digests bytea NOT NULL);

CREATE TABLE unlisted_tree (
    tree bytea PRIMARY KEY,
    first_us bigint NOT NULL,
    last_us bigint NOT NULL);
