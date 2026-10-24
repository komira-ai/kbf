-- kbf-migration: expand
-- The pack index: where each blob lies inside a pack, an object that holds many blobs.
-- Both tables are new, and they live outside kv, feed and the commit head: a transaction
-- that records blobs takes none of the locks a state transaction queues on. Rows of
-- `blob` are only ever inserted by a recording, and touched (last_touch_us) afterwards.
-- Names are unqualified; the runner sets the search path to the store's schema.
--
-- pack: one row per pack object. `key` is the object's key and is unique, so recording a
-- pack twice finds the first row. `blob_count` is how many blobs the pack holds and
-- `live_blobs` how many of them the index pointed at this pack when it was recorded (it is
-- written once): a blob already indexed in another pack keeps its first location, so a pack
-- can hold blobs it has no row for.
-- `created_us` is the store's clock when the pack was recorded.
CREATE TABLE pack (
    pack_id bigserial PRIMARY KEY,
    key text NOT NULL UNIQUE,
    writer text NOT NULL,
    size bigint NOT NULL,
    created_us bigint NOT NULL,
    blob_count integer NOT NULL,
    live_blobs integer NOT NULL);
-- blob: one row per distinct blob (hash and size). `off` is where its bytes start in the
-- pack, and its length is `size`. Nothing but last_touch_us changes in a row after it is
-- written, and it is in no index, so a touch is a heap-only update: the fillfactor leaves
-- room on the page for it.
CREATE TABLE blob (
    hash bytea NOT NULL,
    size bigint NOT NULL,
    pack_id bigint NOT NULL,
    off bigint NOT NULL,
    last_touch_us bigint NOT NULL,
    PRIMARY KEY (hash, size)) WITH (fillfactor = 70);
-- The blobs of a pack: compaction and the question "does any blob point at this pack".
CREATE INDEX blob_pack ON blob (pack_id);
