// Tests the TVM spill round-trip under AOT `jco transpile` (the other jco path
// besides the RuntimeBindgen one run-core.mjs uses). Runs entirely in Node: WASI
// from @bytecodealliance/preview2-shim, the tvm:memory imports from our JS host.
// Asserts the spilling sort returns the correct min=0 (i.e. blocks round-trip
// through host TVM regions without corruption).
//
// Prereq: jco transpile the component into $AOT first (npm run verify-tvm-aot).
import { readFileSync } from 'node:fs'
import { join, basename } from 'node:path'
import { pathToFileURL } from 'node:url'
import { cli, clocks, filesystem, io, random, sockets } from '@bytecodealliance/preview2-shim'
import { createTvmHost, tvmDebugEnabled } from './tvm-host.mjs'

const AOT = process.env.AOT_DIR ?? '/tmp/tvm-aot'
const { instantiate } = await import(pathToFileURL(join(AOT, 'ducklink_core.js')).href)
// Sync Module (works for both sync and async --instantiation modes).
const getCoreModule = (name) => new WebAssembly.Module(readFileSync(join(AOT, basename(name))))

const duckdbStubs = {
  'duckdb:component/host-extension-loader': { requestLoad: () => false },
  'duckdb:component/extension-loader-hooks': {
    getPendingRegistrations: () => ({
      scalars: [], tables: [], aggregates: [], macros: [],
      replacementScans: [], logicalTypes: [], casts: [],
    }),
  },
  'duckdb:extension/callback-dispatch': {
    callScalar: () => { throw new Error('cb') }, callTable: () => { throw new Error('cb') },
    callAggregate: () => { throw new Error('cb') }, callPragma: () => { throw new Error('cb') },
    callCast: () => { throw new Error('cb') },
  },
}

const tvm = createTvmHost({ debug: tvmDebugEnabled() })

const imports = {
  'wasi:cli/environment': cli.environment,
  'wasi:cli/exit': cli.exit,
  'wasi:cli/stderr': cli.stderr,
  'wasi:cli/stdin': cli.stdin,
  'wasi:cli/stdout': cli.stdout,
  'wasi:cli/terminal-input': cli.terminalInput,
  'wasi:cli/terminal-output': cli.terminalOutput,
  'wasi:cli/terminal-stderr': cli.terminalStderr,
  'wasi:cli/terminal-stdin': cli.terminalStdin,
  'wasi:cli/terminal-stdout': cli.terminalStdout,
  'wasi:clocks/monotonic-clock': clocks.monotonicClock,
  'wasi:clocks/wall-clock': clocks.wallClock,
  'wasi:filesystem/preopens': filesystem.preopens,
  'wasi:filesystem/types': filesystem.types,
  'wasi:io/error': io.error,
  'wasi:io/poll': io.poll,
  'wasi:io/streams': io.streams,
  'wasi:random/insecure-seed': random.insecureSeed,
  'wasi:random/random': random.random,
  'wasi:sockets/instance-network': sockets.instanceNetwork,
  'wasi:sockets/ip-name-lookup': sockets.ipNameLookup,
  'wasi:sockets/network': sockets.network,
  'wasi:sockets/tcp-create-socket': sockets.tcpCreateSocket,
  'wasi:sockets/tcp': sockets.tcp,
  'wasi:sockets/udp-create-socket': sockets.udpCreateSocket,
  'wasi:sockets/udp': sockets.udp,
  ...duckdbStubs,
  ...tvm.imports,
}

const inst = await instantiate(getCoreModule, imports)
const db = inst.database
const conn = db.open(undefined) // in-memory

db.execute(conn, "SET memory_limit='64MB'") // low budget -> forces spill
db.execute(conn, 'SET threads=1')           // no temp_directory: TVM-only spill
const res = db.execute(
  conn,
  'SELECT count(*) AS n, min(i) AS lo, max(i) AS hi ' +
    'FROM (SELECT i FROM range(10000000) t(i) ORDER BY i DESC) sub',
)
db.close(conn)

const row = res.rows[0]
const [n, lo, hi] = [row[0].val, row[1].val, row[2].val]
const stats = tvm.stats()
const correct = String(n) === '10000000' && String(lo) === '0' && String(hi) === '9999999'
console.log('result:', JSON.stringify(res.rows, (_, v) => (typeof v === 'bigint' ? `${v}n` : v)))
console.log(`correct (n=10M, min=0, max=9999999): ${correct}`)
console.log(`tvm regions=${stats.regionsOpened} written=${(stats.bytesWritten / (1 << 20)).toFixed(1)}MiB read=${(stats.bytesRead / (1 << 20)).toFixed(1)}MiB`)
console.log(correct ? 'PASS: AOT spill round-trip correct' : 'FAIL: AOT spill round-trip corrupt')
process.exit(correct ? 0 : 1)
