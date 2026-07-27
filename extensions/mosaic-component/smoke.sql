-- Smoke test for the `mosaic` component.
--
-- Scalar-only assertion: exercises the pure-computation surface. The
-- canonical `mosaic_create(...)` scalar now works directly via nested-exec
-- (Phase 4 shared-ExtensionManager sibling; ADR §Decision 6), but it
-- writes to `__mosaic_apps` + `routes` tables and is exercised end-to-end
-- by `scripts/mosaic-phase1-e2e.sh`.
--
-- This smoke stays pure so it runs on the default `:memory:` harness
-- without any table setup. The only fully pure scalar in the surface is
-- `mosaic_plot_spec(sql, kind, opts_json)` — a deterministic vgplot spec
-- generator with no nested-exec, filesystem, or network. Each supported
-- `kind` (line|bar|dot|area) gets a substring check on the returned
-- JSON to prove the scalar dispatched and the mark type was honored.
SELECT (mosaic_plot_spec('SELECT 1 x, 2 y',
                         'line',
                         '{"x":"x","y":"y"}')
        LIKE '%"mark":"line"%') AS plot_kind_line;
SELECT (mosaic_plot_spec('SELECT 1 x, 2 y',
                         'bar',
                         '{"x":"x","y":"y"}')
        LIKE '%"mark":"bar"%') AS plot_kind_bar;
SELECT (mosaic_plot_spec('SELECT 1 x, 2 y',
                         'dot',
                         '{"x":"x","y":"y"}')
        LIKE '%"mark":"dot"%') AS plot_kind_dot;
SELECT (mosaic_plot_spec('SELECT 1 x, 2 y',
                         'area',
                         '{"x":"x","y":"y"}')
        LIKE '%"mark":"area"%') AS plot_kind_area;
