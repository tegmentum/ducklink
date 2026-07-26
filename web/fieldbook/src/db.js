// Wrapper around the WIT-based ducklink DuckDB core (from ~/git/duckdb-wasm)
// — the same wasip2 component the native `ducklink` binary embeds via
// wasmtime, transpiled + run in the browser via jco + @tegmentum/wasi-polyfill.
//
// Pattern lifted from `web/run-core.mjs` + `web/browser-ext-entry.mjs` (the
// prior art that already runs sample_extension.wasm end-to-end in-browser).
// The `../tvm-host.mjs` full-featured spill host is imported directly (pure
// JS, no npm deps) rather than reimplemented.
//
// Phase 1: we deliberately do NOT load fieldbook.wasm — the notebook talks
// direct SQL to `__fieldbook_*` tables (same trick the fieldbook-dotcmd
// component uses, see `extensions/fieldbook-dotcmd/src/lib.rs`). That
// side-steps the nested-exec re-entry trap the fieldbook engine scalars hit
// in the same-process wasm-DuckDB world. fieldbook.wasm is still copied
// into dist/ so a Phase 2 upgrade can wire it in without changing the
// bundle URL layout.
import { createRuntimeBindgen } from '@tegmentum/wasi-polyfill/wasip2/runtime'
import { Polyfill, AllowAllPolicy } from '@tegmentum/wasi-polyfill/wasip2'
import * as cli from '@tegmentum/wasi-polyfill/wasip2/plugins/cli'
import * as io from '@tegmentum/wasi-polyfill/wasip2/plugins/io'
import * as fs from '@tegmentum/wasi-polyfill/wasip2/plugins/filesystem'
import * as clocks from '@tegmentum/wasi-polyfill/wasip2/plugins/clocks'
import * as random from '@tegmentum/wasi-polyfill/wasip2/plugins/random'
import * as sockets from '@tegmentum/wasi-polyfill/wasip2/plugins/sockets'
import { createTvmHost, tvmDebugEnabled } from '../../tvm-host.mjs'

// Path inside the polyfill's in-memory FS where the notebook keeps its
// backing DuckDB file. A concrete path (rather than an unnamed in-memory
// DB) means the "download .duckdb" button can just read the file bytes
// straight out of the memfs after a CHECKPOINT.
export const DB_PATH = '/fieldbook.duckdb'

function configurePolyfill() {
  class FsPolicy extends AllowAllPolicy {
    configure(iface) {
      const cfg = super.configure(iface)
      if (iface.package === 'wasi:filesystem') {
        cfg.implementation = 'memory'
        cfg.options = {
          ...(cfg.options || {}),
          preopens: [{ path: '/' }],
          mkdirs: ['/.duckdb'],
        }
      }
      // The core links socket-using extensions unconditionally so it imports
      // wasi:sockets; the default (virtual) plugins satisfy those. The
      // notebook itself never touches the network.
      return cfg
    }
  }
  const polyfill = new Polyfill({ policy: new FsPolicy() })
  for (const p of [
    cli.environmentPlugin,
    cli.exitPlugin, cli.stdoutPlugin, cli.stderrPlugin, cli.stdinPlugin,
    cli.terminalInputPlugin, cli.terminalOutputPlugin, cli.terminalStdinPlugin,
    cli.terminalStdoutPlugin, cli.terminalStderrPlugin,
    io.streamsPlugin, io.pollPlugin, io.errorPlugin,
    fs.filesystemTypesPlugin, fs.filesystemPreopensPlugin,
    clocks.monotonicClockPlugin, clocks.wallClockPlugin,
    random.randomPlugin, random.insecureRandomPlugin, random.insecureSeedPlugin,
    ...sockets.socketPlugins,
  ]) {
    polyfill.registerPlugin(p)
  }
  return polyfill
}

// The core's custom `duckdb:*` imports. Nothing to load in Phase 1 (no
// extension host wired) — stubs report "no extensions" and dispatch throws
// if the core ever tried to route through them. Sourced from
// `web/run-core.mjs::duckdbStubImports` / `hostProviderStubs` — kept
// verbatim so the two entry points stay in lock-step.
function duckdbStubImports() {
  return {
    'duckdb:component/host-extension-loader': { requestLoad: () => false },
    'duckdb:component/extension-loader-hooks': {
      getPendingRegistrations: () => ({
        scalars: [], tables: [], aggregates: [], macros: [],
        replacementScans: [], logicalTypes: [], casts: [],
      }),
    },
    'duckdb:extension/callback-dispatch': {
      callScalar: () => { throw new Error('no extension loaded') },
      callScalarBatch: () => { throw new Error('no extension loaded') },
      callTable: () => { throw new Error('no extension loaded') },
      callAggregate: () => { throw new Error('no extension loaded') },
      callPragma: () => { throw new Error('no extension loaded') },
      callCast: () => { throw new Error('no extension loaded') },
    },
    'duckdb:extension/collation-host': { collationList: () => [] },
    'duckdb:extension/pragma-host': { pragmaList: () => [] },
    'duckdb:extension/storage-host': {
      storageListTypes: () => [],
      storageAttach: () => { throw new Error('no storage backend') },
      storageListTables: () => { throw new Error('no storage backend') },
      storageTableColumns: () => { throw new Error('no storage backend') },
      storageScanOpen: () => { throw new Error('no storage backend') },
      storageScanNext: () => { throw new Error('no storage backend') },
      storageScanClose: () => { throw new Error('no storage backend') },
    },
    'duckdb:extension/index-host': {
      indexTypeList: () => [],
      indexCreate: () => { throw new Error('no index backend') },
      indexAppend: () => { throw new Error('no index backend') },
      indexBuild: () => { throw new Error('no index backend') },
      indexSearch: () => { throw new Error('no index backend') },
      indexDrop: () => { throw new Error('no index backend') },
    },
    'duckdb:extension/files-host': {
      fileOpen: () => { throw new Error('no host file backend') },
      fileRead: () => { throw new Error('no host file backend') },
      fileClose: () => { throw new Error('no host file backend') },
    },
  }
}

// Cache of the polyfill's FS instance so `snapshotFileBytes` can reach into
// the same in-memory FS the core is running against.
let _polyfillFsInstance = null

export async function instantiateCore(componentBytes) {
  const polyfill = configurePolyfill()
  const tvm = createTvmHost({ debug: tvmDebugEnabled() })
  const bindgen = createRuntimeBindgen({
    polyfill,
    additionalImports: { ...duckdbStubImports(), ...tvm.imports },
    // JSPI: the core's `execute` blocks on wasi:io/poll via the wasi:io
    // stack. Promote the poll imports to suspending and the execute export
    // to promising so the async event loop keeps ticking — this is what
    // `web/run-core.mjs` does and what `browser-ext-entry.mjs` proved
    // works end-to-end.
    jcoOptions: {
      asyncMode: 'jspi',
      asyncImports: [
        'wasi:io/poll@0.2.6#[method]pollable.block',
        'wasi:io/poll@0.2.6#poll',
      ],
      asyncExports: ['duckdb:component/database#execute'],
    },
  })
  const instance = await bindgen.instantiate(componentBytes)
  const root = instance.exports ?? instance
  // Cache the polyfill's FS instance for the download button.
  _polyfillFsInstance = fs.getGlobalFilesystemInstance()
  const database = root.database
  database.__tvmHost = tvm
  return database
}

// Read the DuckDB file bytes straight out of the polyfill's in-memory FS.
// Call `CHECKPOINT` first so the WAL is flushed. Returns null if the file
// doesn't exist (open-on-first-write hasn't run yet).
export function snapshotFileBytes(path = DB_PATH) {
  if (!_polyfillFsInstance) return null
  const memfs = _polyfillFsInstance.getFileSystem()
  const res = memfs.getNode(path)
  if (!res || res.tag === 'err') return null
  const node = res.val
  if (!node || node.type !== 'file') return null
  // `content` is a Uint8Array view into the memfs; copy so the download
  // Blob doesn't alias live memory.
  return new Uint8Array(node.content)
}
