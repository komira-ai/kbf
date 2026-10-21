-- kbf-migration: expand
-- The state store's tables (docs/design/architecture-v2.md section 4.3): the key/value
-- rows, the change feed, the counters, the commit head and the revision sequence. Names
-- are unqualified; the runner sets the search path to the store's schema.
CREATE TABLE kv (
    key text COLLATE "C" PRIMARY KEY,
    value bytea NOT NULL,
    version bigint NOT NULL,
    expires_us bigint);
CREATE INDEX kv_expiry ON kv (expires_us) WHERE expires_us IS NOT NULL;
CREATE TABLE feed (
    rev bigint PRIMARY KEY,
    key text NOT NULL,
    kind smallint NOT NULL,
    value bytea);
CREATE TABLE counters (
    name text PRIMARY KEY,
    value bigint NOT NULL);
CREATE TABLE head (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    rev bigint NOT NULL);
INSERT INTO head (rev) VALUES (0);
CREATE SEQUENCE rev_seq;
