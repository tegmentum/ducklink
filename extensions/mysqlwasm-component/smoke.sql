-- mysqlwasm storage backend smoke test.
--
-- REQUIRES a live MariaDB/MySQL server on 127.0.0.1:3306 (user `duck`,
-- password `duckpw`, database `ducktest`, table t(a INT, b VARCHAR) with rows
-- (1,'x'),(2,'y')) AND the network grant: run with
--   DUCKLINK_NETWORK_GRANT=mysqlwasm
-- This is NOT part of smoke.py --all (it needs the external server + grant), so
-- it is intentionally NOT registered in registry/index.json.
--
-- Note: the type is `mysqlwasm`, not `mysql`: the prebuilt lean core links the
-- NATIVE mysql_scanner, whose StorageExtension owns the `mysql` ATTACH type and
-- spawns a connection thread that aborts on wasm ("thread constructor failed").
-- The `mysqlwasm` alias routes ATTACH straight to this wasm component.
LOAD mysqlwasm;
ATTACH 'host=127.0.0.1 port=3306 user=duck password=duckpw database=ducktest' AS db (TYPE mysqlwasm);
SELECT a, b FROM db.t ORDER BY a;
SELECT a FROM db.t WHERE a > 1;
