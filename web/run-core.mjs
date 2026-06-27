// Run the DuckDB core Wasm component via @tegmentum/wasi-polyfill.
//
// The same wasip2 component the native `duckdb-host` runs is instantiated here
// with WASI provided by the polyfill's plugins and the custom `duckdb:*` host
// imports stubbed (no extension loading yet). `RuntimeBindgen` transpiles the
// component with jco's browser build and wires the polyfill's import providers.
//
// This is browser-targeted: `RuntimeBindgen` imports the jco-generated module
// from a `blob:` URL, which works in browsers but NOT Node's ESM loader. Use it
// from a browser bundle (see README.md); a Node run reaches transpilation and
// then fails on `import(blob:)`.
import { createRuntimeBindgen } from '@tegmentum/wasi-polyfill/wasip2/runtime'
import { Polyfill, AllowAllPolicy } from '@tegmentum/wasi-polyfill/wasip2'
import * as cli from '@tegmentum/wasi-polyfill/wasip2/plugins/cli'
import * as io from '@tegmentum/wasi-polyfill/wasip2/plugins/io'
import * as fs from '@tegmentum/wasi-polyfill/wasip2/plugins/filesystem'
import * as clocks from '@tegmentum/wasi-polyfill/wasip2/plugins/clocks'
import * as random from '@tegmentum/wasi-polyfill/wasip2/plugins/random'
import * as sockets from '@tegmentum/wasi-polyfill/wasip2/plugins/sockets'
import { createTvmHost, tvmDebugEnabled } from './tvm-host.mjs'

export function configurePolyfill() {
  // The http extension's HTTP client (std TcpStream + read_to_end) drains the
  // socket with the non-blocking `input-stream.read` in a tight loop, treating
  // an empty read as "retry" without re-arming a poll. Over the ws-gateway TCP
  // tunnel that spin starves the browser event loop, so the next WebSocket
  // frame / the connection-close EOF never arrives and the read loops forever.
  // Opt into the polyfill's empty-read yield: on an empty read it returns a
  // Promise that yields a macrotask (letting pending WS frames land) and then
  // re-reads. Needs the JSPI transpile to mark `input-stream.read` async (see
  // extension-host.mjs asyncImports); without that the env/flag is a no-op.
  io.setAsyncReadYield(true)

  // Plugin config comes from the policy (registerPlugin's config arg is ignored).
  // Allow-all (dev) + a writable in-memory filesystem: the preopens plugin
  // otherwise defaults to "empty" (no preopens), leaving DuckDB with no writable
  // directory. A "/" preopen plus a pre-created ~/.duckdb (DuckDB's
  // CreateDirectory is non-recursive) let it create its extension dir on LOAD.
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
      if (iface.package === 'wasi:sockets') {
        // Real browser networking for socket-using extensions (dns, http, ...):
        //  - ip-name-lookup -> DNS-over-HTTPS (server-free; CORS via Cloudflare).
        //    `localhost` is statically `::1` (the dns smoke expects IPv6 loopback).
        //  - tcp -> the ws-gateway WebSocket proxy (the http extension does its own
        //    TLS over raw TCP, so fetch can't substitute; the gateway relays raw
        //    bytes, TLS stays end-to-end). The driver sets __WS_GATEWAY_URL__.
        // Non-network extensions never touch these, so this is otherwise inert.
        const gatewayUrl =
          (typeof globalThis !== 'undefined' && globalThis.__WS_GATEWAY_URL__) ||
          'ws://localhost:8080'
        if (iface.name === 'ip-name-lookup') {
          cfg.implementation = 'doh'
          cfg.options = { ...(cfg.options || {}), staticMappings: { localhost: ['::1'] } }
        } else if (iface.name === 'tcp' || iface.name === 'tcp-create-socket') {
          cfg.implementation = 'tunneled'
          cfg.options = { ...(cfg.options || {}), gatewayUrl }
        }
      }
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
    // The core links socket-using extensions (httpfs/postgres/mysql), so it
    // imports the wasi:sockets interfaces and won't instantiate without them.
    // Register the default (virtual) socket plugins; plain/spill queries never
    // touch the network, they just need the imports satisfied.
    ...sockets.socketPlugins,
  ]) {
    polyfill.registerPlugin(p)
  }
  return polyfill
}

// jco camelCases method + record-field names; these stubs satisfy the core's
// custom imports for plain queries (no extension loading).
export const duckdbStubImports = {
  'duckdb:component/host-extension-loader': { requestLoad: () => false },
  'duckdb:component/extension-loader-hooks': {
    getPendingRegistrations: () => ({
      scalars: [], tables: [], aggregates: [], macros: [],
      replacementScans: [], logicalTypes: [], casts: [],
    }),
  },
  'duckdb:extension/callback-dispatch': {
    callScalar: () => { throw new Error('callbacks unavailable') },
    callScalarBatch: () => { throw new Error('callbacks unavailable') },
    callTable: () => { throw new Error('callbacks unavailable') },
    callAggregate: () => { throw new Error('callbacks unavailable') },
    callPragma: () => { throw new Error('callbacks unavailable') },
    callCast: () => { throw new Error('callbacks unavailable') },
  },
  // Rich-types host callbacks the core enumerates on load (collations, pragmas,
  // custom storage backends, index types, host files). The plain-query path
  // registers none of these, so the stubs report "nothing registered". The
  // extension host (coreImports) overrides these when an extension is loaded.
  ...hostProviderStubs(),
}

// Empty implementations of the core's rich-types host interfaces. Shared by the
// plain-query stubs and the extension host so the core instantiates whether or
// not an extension contributes collations/pragmas/storage/index/file backends.
export function hostProviderStubs() {
  return {
    'duckdb:extension/collation-host': {
      collationList: () => [],
    },
    'duckdb:extension/pragma-host': {
      pragmaList: () => [],
    },
    'duckdb:extension/storage-host': {
      storageListTypes: () => [],
      storageAttach: () => { throw new Error('no storage backend registered') },
      storageListTables: () => { throw new Error('no storage backend registered') },
      storageTableColumns: () => { throw new Error('no storage backend registered') },
      storageScanOpen: () => { throw new Error('no storage backend registered') },
      storageScanNext: () => { throw new Error('no storage backend registered') },
      storageScanClose: () => { throw new Error('no storage backend registered') },
    },
    'duckdb:extension/index-host': {
      indexTypeList: () => [],
      indexCreate: () => { throw new Error('no index backend registered') },
      indexAppend: () => { throw new Error('no index backend registered') },
      indexBuild: () => { throw new Error('no index backend registered') },
      indexSearch: () => { throw new Error('no index backend registered') },
      indexDrop: () => { throw new Error('no index backend registered') },
    },
    'duckdb:extension/files-host': {
      fileOpen: () => { throw new Error('no host file backend registered') },
      fileRead: () => { throw new Error('no host file backend registered') },
      fileClose: () => { throw new Error('no host file backend registered') },
    },
  }
}

// Instantiate the core component. `additionalImports` defaults to the stubs;
// pass an extension host's `coreImports()` to enable extension loading. The TVM
// host (tvm:memory imports) is always merged in -- the core imports it
// unconditionally, so the component fails to instantiate without it, and it
// backs DuckDB's larger-than-memory spill. The host is reachable afterwards as
// `db.__tvmHost` for tests/inspection (region + byte-transfer stats).
export async function instantiateCore(componentBytes, additionalImports = duckdbStubImports) {
  const polyfill = configurePolyfill()
  const tvm = createTvmHost({ debug: tvmDebugEnabled() })
  const bindgen = createRuntimeBindgen({
    polyfill,
    additionalImports: { ...additionalImports, ...tvm.imports },
    // JSPI: socket-using extensions (dns, http) block the guest on async I/O
    // (DoH fetch / ws-gateway WebSocket) via wasi:io/poll. In the default sync
    // mode the guest can't suspend on the polyfill's async plugins (deadlock), so
    // promote the blocking poll imports to suspending and the DuckDB query export
    // (`execute`, which transitively polls) to promising. Requires host JSPI
    // (Chrome 137+ / the bundled Playwright Chromium has it).
    jcoOptions: {
      asyncMode: 'jspi',
      asyncImports: [
        'wasi:io/poll@0.2.6#[method]pollable.block',
        'wasi:io/poll@0.2.6#poll',
        // The scalar callback runs in the extension component, which does the
        // async socket I/O. The extension's scalar export is promised (see
        // extension-host.mjs) and the JS bridge awaits it, so this import
        // returns a Promise; promote it to suspending so the core's `execute`
        // stack yields across the whole nested DoH/ws-gateway round-trip.
        'duckdb:extension/callback-dispatch#call-scalar-batch',
      ],
      asyncExports: ['duckdb:component/database#execute'],
    },
  })
  const instance = await bindgen.instantiate(componentBytes)
  const root = instance.exports ?? instance
  const database = root.database
  database.__tvmHost = tvm
  return database
}

// Instantiate the core component and run a query (no extension loading).
export async function runQuery(componentBytes, sql = 'SELECT 42 AS answer, 1 + 1 AS two') {
  const db = await instantiateCore(componentBytes)
  const conn = db.open(undefined) // none -> in-memory database
  const result = await db.execute(conn, sql) // execute is JSPI-promised (async)
  db.close(conn)
  return result
}

// Node entry point — reaches transpilation, then fails on `import(blob:)` (a
// browser-only scheme). Run this from the browser bundle instead.
if (typeof process !== 'undefined' && import.meta.url === `file://${process.argv[1]}`) {
  const { readFile } = await import('node:fs/promises')
  const wasm = new Uint8Array(
    await readFile(new URL('../target/wasm32-wasip2/release/ducklink_core.wasm', import.meta.url)),
  )
  const result = await runQuery(wasm)
  console.log('columns:', result.columns.map((c) => c.name))
  console.log(
    'rows:',
    JSON.stringify(result.rows, (_, v) => (typeof v === 'bigint' ? `${v}n` : v)),
  )
}
