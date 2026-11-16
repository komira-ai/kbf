-- kbf-migration: expand
-- The action cache: the result of an action, by the action's digest, in the state store
-- beside the pack index. The table is new, and like the pack index's it lives outside kv, feed
-- and the commit head, so a write takes none of the locks a state transaction queues on.
-- Names are unqualified; the runner sets the search path to the store's schema.
--
-- action_result: one row per action digest (`hash` and `size`, the action's own digest).
-- `result` is the encoded ActionResult kept inline: a small record (exit code, the digests
-- of stdout and stderr, the output files with their digests), never the outputs themselves,
-- which are blobs of the pack index. A result that encodes to no bytes is a valid result,
-- so `result` may be empty. A later write for the same action replaces the row (the last
-- writer wins, as UpdateActionResult does); `written_us` is the store's clock at that write.
-- The row is written in the transaction that records the packs of the action's outputs, so
-- a row never names a blob whose pack is not recorded.
CREATE TABLE action_result (
    hash bytea NOT NULL,
    size bigint NOT NULL,
    result bytea NOT NULL,
    written_us bigint NOT NULL,
    PRIMARY KEY (hash, size));
