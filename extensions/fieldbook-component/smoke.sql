-- Smoke test for the `fieldbook` component: create a book, add an entry,
-- verify it landed. End-to-end execution of an entry belongs to the CLI
-- orchestrator (see the fieldbook-implementation-status memory) and is
-- out of scope for this component's smoke -- the wasm engine only owns
-- read + mutate + record. Requires the host's `nested-exec` import
-- (task #7) to be wired up.
SELECT fieldbook_create('smoke') AS created;
SELECT fieldbook_add_entry('smoke', 'CREATE TABLE t AS SELECT 1 x') AS ordinal;
SELECT count(*) AS n FROM fieldbook_entries('smoke');
SELECT fieldbook_source('smoke', 1) AS src;
SELECT fieldbook_drop('smoke') AS dropped;
