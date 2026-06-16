# Preview2 Filesystem Adapter for DuckDB

DuckDB's WASI build still traps when probing the database path. We need a proper 
Preview2 filesystem implementation instead of relying on WASI-libc shims. Breakdown:

- [x] Build a translation layer from `wasi:filesystem` (0.2.8) to the subset of libc
      calls DuckDB uses (`open`, `stat`, glob/dir iteration, temp files, etc.).
- [x] Hook the adapter into DuckDB's `FileSystem::OpenFile` / `MagicBytes::CheckMagicBytes`
      path so all file access goes through CDI preview2 APIs.
- [x] Handle error mapping and resource cleanup (descriptors, capability checks).
- [x] Wire the adapter into the component so we export/import the correct WIT types.
- [x] Add a smoke test that runs the CLI component against an on-disk database file
      under Wasmtime preview2 via `scripts/smoke-cli.sh` (set `ON_DISK_SMOKE=1`).

Create a feature branch once design/options are clear.
