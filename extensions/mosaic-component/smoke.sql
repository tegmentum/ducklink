-- Smoke test for the `mosaic` component.
--
-- Exercises the pure-computation surface — the URL / spec / SQL builders
-- that don't need nested-exec. mosaic_create is the canonical API but
-- it currently traps against the standalone ducklink CLI's nested-exec
-- re-entry (same limitation the fieldbook_* mutate scalars hit today);
-- see docs/mosaic-phase0-findings.md §1.2 + README for the workaround
-- via mosaic_install_sql + POST /sql (which the E2E script
-- scripts/mosaic-phase1-e2e.sh drives end-to-end).
--
-- `mosaic_install_sql(name, spec_json, opts_json)` returns the semicolon-
-- batched SQL an operator can POST /sql back to install the app. It's
-- pure -- no nested-exec, no filesystem, no network -- so it's the
-- ideal smoke surface for the scalar-only assertion this test runs.
SELECT (mosaic_install_sql('demo',
                           '{"data":{"fixture":{"query":"SELECT 1"}},"plot":[]}',
                           '{"token":"deadbeefdeadbeef"}')
        LIKE '%INSERT INTO __mosaic_apps%') AS has_apps_insert;
SELECT (mosaic_install_sql('demo',
                           '{"data":{}}',
                           '{"token":"deadbeefdeadbeef"}')
        LIKE '%/ducklink/mosaic/app/demo%') AS has_app_route;
SELECT (mosaic_install_sql('demo',
                           '{"data":{}}',
                           '{"token":"deadbeefdeadbeef"}')
        LIKE '%mosaicRuntime.boot%') AS embeds_bundle_boot;
-- mosaic_plot_spec (pure): SQL + kind + {x,y} -> valid JSON spec.
SELECT (mosaic_plot_spec('SELECT 1 x, 2 y',
                         'line',
                         '{"x":"x","y":"y"}')
        LIKE '%"mark":"line"%') AS plot_kind_line;
