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
import { createDevPolyfill } from '@tegmentum/wasi-polyfill/wasip2'
import * as cli from '@tegmentum/wasi-polyfill/wasip2/plugins/cli'
import * as io from '@tegmentum/wasi-polyfill/wasip2/plugins/io'
import * as fs from '@tegmentum/wasi-polyfill/wasip2/plugins/filesystem'
import * as clocks from '@tegmentum/wasi-polyfill/wasip2/plugins/clocks'
import * as random from '@tegmentum/wasi-polyfill/wasip2/plugins/random'

export function configurePolyfill() {
  const polyfill = createDevPolyfill()
  polyfill.registerPlugin(cli.environmentPlugin, {
    implementation: 'virtual',
    environment: {},
    args: ['duckdb-core'],
  })
  for (const p of [
    cli.exitPlugin, cli.stdoutPlugin, cli.stderrPlugin, cli.stdinPlugin,
    cli.terminalInputPlugin, cli.terminalOutputPlugin, cli.terminalStdinPlugin,
    cli.terminalStdoutPlugin, cli.terminalStderrPlugin,
    io.streamsPlugin, io.pollPlugin, io.errorPlugin,
    fs.filesystemPreopensPlugin, fs.filesystemTypesPlugin,
    clocks.monotonicClockPlugin, clocks.wallClockPlugin,
    random.randomPlugin, random.insecureRandomPlugin, random.insecureSeedPlugin,
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
    callTable: () => { throw new Error('callbacks unavailable') },
    callAggregate: () => { throw new Error('callbacks unavailable') },
    callPragma: () => { throw new Error('callbacks unavailable') },
    callCast: () => { throw new Error('callbacks unavailable') },
  },
}

// Instantiate the core component. `additionalImports` defaults to the stubs;
// pass an extension host's `coreImports()` to enable extension loading.
export async function instantiateCore(componentBytes, additionalImports = duckdbStubImports) {
  const polyfill = configurePolyfill()
  const bindgen = createRuntimeBindgen({ polyfill, additionalImports })
  const instance = await bindgen.instantiate(componentBytes)
  const root = instance.exports ?? instance
  return root.database
}

// Instantiate the core component and run a query (no extension loading).
export async function runQuery(componentBytes, sql = 'SELECT 42 AS answer, 1 + 1 AS two') {
  const db = await instantiateCore(componentBytes)
  const conn = db.open(undefined) // none -> in-memory database
  const result = db.execute(conn, sql)
  db.close(conn)
  return result
}

// Node entry point — reaches transpilation, then fails on `import(blob:)` (a
// browser-only scheme). Run this from the browser bundle instead.
if (typeof process !== 'undefined' && import.meta.url === `file://${process.argv[1]}`) {
  const { readFile } = await import('node:fs/promises')
  const wasm = new Uint8Array(
    await readFile(new URL('../target/wasm32-wasip2/release/duckdb_core_component.wasm', import.meta.url)),
  )
  const result = await runQuery(wasm)
  console.log('columns:', result.columns.map((c) => c.name))
  console.log('rows:', JSON.stringify(result.rows))
}
