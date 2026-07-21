# Ducklink conformance suite — workspace mirror

**This is a MIRROR** of the authoritative conformance suite that
lives in the ducklink-extension repo:
[`ducklink-extension/conformance/`](https://github.com/tegmentum/ducklink-extension/tree/v5.0.0/conformance).

The `scripts/*.sql` and `expected/*.out` files here MUST stay
byte-identical to the upstream ones. Any change to the surface (a
new committed entry point, a renamed view) lands in
ducklink-extension first, then this mirror re-syncs. See
`STABILITY.md § 4` in ducklink-extension for the policy.

## Why a mirror

The ducklink workspace and the ducklink-extension are two hosts of
the same SQL surface — see
[`STABILITY.md § 1.1`](https://github.com/tegmentum/ducklink-extension/blob/v5.0.0/STABILITY.md).
Both are supposed to bind identically-shaped `ducklink_load`,
`ducklink_prefix`, `PREFIX`, `ducklink_version`, `ducklink_help`
and produce identical rows for the ten `ducklink.*` discovery
entries.

Mirroring puts the reference within reach of the workspace's own
CI and its `.read`-driven CLI test path, so drift shows up here
too, not only in the extension repo.

## Current status: the workspace does NOT satisfy the suite

As of this mirror, the workspace host (`crates/ducklink-host`) does
not ship the SQL entry points or the `ducklink.*` schema. The
control-plane surface is CLI dotcommands (`.prefix`, `.bundle`,
`.tables`, `.greet`) with no SQL-callable equivalent. Every script
in this suite will fail against the current CLI.

That is the whole point. The gap between "what the extension
commits to" and "what the workspace provides today" is now
concretely enumerated:

- 5 SQL entry points (§ 1.1) missing
- 10 discovery entries (§ 1.2) missing

Closing the gap is the workspace port work, tracked separately.

## Running the suite (once the surface is ported)

The suite is designed to be run through the workspace CLI's
`.read` + `.output` machinery. A shell driver:

```
./run.sh
```

drives each `scripts/NN-*.sql` through the CLI, captures the output
into `actuals/NN-*.out`, and diffs against `expected/NN-*.out`.
Failures print the diff and exit non-zero.

The driver depends on:

- A built `ducklink` binary at `../target/release/ducklink` (or on
  `$PATH`).
- The CLI's `.read`, `.output`, and `.print` dotcommands (all
  committed as of `16a93a9`).

### Known runner limitation

The workspace `ducklink` host wrapper forwards CLI_ARGS through
`--` into the wasm CLI component, but that component treats the
first positional argument as a database PATH. Both `-c "SQL"` and
`-init file.sql` invocations currently bind the driver text as a
filesystem path and fail. Getting `run.sh` to actually execute
needs a workspace-CLI change — a batch-mode flag that unambiguously
runs a script, or `-c` semantics that never look like a path. That
work is a prerequisite for the conformance suite to run against
this host; it's not a change to the suite itself.

## Re-syncing from upstream

When ducklink-extension lands a new committed surface, rerun:

```
rsync -a --delete <path-to-ducklink-extension>/conformance/scripts/  ./scripts/
rsync -a --delete <path-to-ducklink-extension>/conformance/expected/ ./expected/
```

Commit the re-sync with the ducklink-extension release it tracks
(e.g. `chore(conformance): sync to ducklink-extension v5.1.0`).
