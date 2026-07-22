-- postgis_core smoke: prove LOAD + one ST_* dispatch through the
-- compose:dynlink/linker into the resident postgis_core-composed provider.
-- Requires the host to be started with:
--   DUCKLINK_PROVIDERS=postgis_core-composed=/path/to/postgis-core-provider-composed.wasm
-- (see datafission/extensions/postgis/deps/postgis-core-provider-composed.wasm;
--  the `-provider-` variant exports compose:dynlink/endpoint, the bare
--  `-composed` variant does not and traps at resolve time).
LOAD postgis_core;
SELECT ST_AsText(ST_GeomFromText('POINT(1 2)')) AS wkt;
