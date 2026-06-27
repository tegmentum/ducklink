// In-browser extension host: loads a DuckDB extension *component* via jco +
// wasi-polyfill, drives its `load()` (capturing registrations through JS
// implementations of the `duckdb:extension/{runtime,catalog,files}` imports),
// and exposes the `duckdb:*` imports the *core* component needs
// (host-extension-loader / extension-loader-hooks / callback-dispatch).
//
// Component instantiation is async but the core calls `request-load`
// synchronously, so extensions are PRE-LOADED; `request-load` then returns the
// cached result and `call-*` dispatch synchronously to the loaded instance.
import { createRuntimeBindgen } from '@tegmentum/wasi-polyfill/wasip2/runtime'
import { configurePolyfill, hostProviderStubs } from './run-core.mjs'

export function createExtensionHost() {
  // name -> { instance, pending }
  const loaded = new Map()
  let drained = false

  // Builds the JS `duckdb:extension/*` imports the extension component needs.
  // Registration calls accumulate into `pending` (shaped like the core's
  // extension-loader-hooks pending-registrations record, camelCased for jco).
  function buildExtensionImports(pending) {
    let nextId = 1
    const id = () => nextId++
    // Map each table-function registration handle to its name, so a replacement
    // scan can resolve the handle it was given back to a function name. Handles
    // are drawn from the shared id() counter (scalars/aggregates/etc. consume it
    // too), so a name lookup is required rather than index arithmetic.
    const tableHandles = new Map()

    class ScalarCallback { constructor(h) { this.handle = h } }
    class TableCallback { constructor(h) { this.handle = h } }
    class AggregateCallback { constructor(h) { this.handle = h } }
    class PragmaCallback { constructor(h) { this.handle = h } }
    class CastCallback { constructor(h) { this.handle = h } }

    class ScalarRegistry {
      register(name, args, returns, cb, options) {
        pending.scalars.push({ name, arguments: args, returns, callbackHandle: cb.handle, options })
        return id()
      }
    }
    class TableRegistry {
      register(name, args, columns, cb, options) {
        const handle = id()
        pending.tables.push({ name, arguments: args, columns, callbackHandle: cb.handle, options })
        tableHandles.set(handle, name)
        return handle
      }
    }
    class AggregateRegistry {
      register(name, args, returns, cb, options) {
        pending.aggregates.push({ name, arguments: args, returns, callbackHandle: cb.handle, options })
        return id()
      }
    }
    class PragmaRegistry {
      registerCall() { return id() }
    }
    class MacroRegistry {
      registerScalar() { return true }
    }

    const runtime = {
      ScalarCallback, TableCallback, AggregateCallback, PragmaCallback, CastCallback,
      ScalarRegistry, TableRegistry, AggregateRegistry, PragmaRegistry, MacroRegistry,
      getCapability(kind) {
        switch (kind) {
          case 'scalar': return { tag: 'scalar', val: new ScalarRegistry() }
          case 'table': return { tag: 'table', val: new TableRegistry() }
          case 'aggregate': return { tag: 'aggregate', val: new AggregateRegistry() }
          case 'pragma': return { tag: 'pragma', val: new PragmaRegistry() }
          case 'macro': return { tag: 'macro', val: new MacroRegistry() }
          default: return undefined
        }
      },
      listCapabilities: () => ['scalar', 'table', 'aggregate', 'pragma', 'macro'],
    }

    const catalog = {
      // jco shares the runtime CastCallback class with catalog (catalog uses it).
      CastCallback,
      registerLogicalType(ty) {
        pending.logicalTypes.push({ name: ty.name, physical: ty.physical })
        return id()
      },
      registerCast(spec, cb) {
        pending.casts.push({ source: spec.from, target: spec.to, callbackHandle: cb.handle })
      },
      registerMacro(def) {
        pending.macros.push({
          schema: def.schema, name: def.name,
          parameters: def.parameters, definitionSql: def.definitionSql,
        })
      },
    }

    const files = {
      registerReplacementScan(scan) {
        // scan.tableFunction is the handle a TableRegistry.register() returned;
        // resolve it to the function name the host needs.
        pending.replacementScans.push({
          extensions: scan.extensions,
          functionName: tableHandles.get(scan.tableFunction) ?? '',
        })
        return id()
      },
      registerCopyHandler() {
        throw new Error('copy handlers are not supported')
      },
    }

    return {
      'duckdb:extension/runtime': runtime,
      'duckdb:extension/catalog': catalog,
      'duckdb:extension/files': files,
      'duckdb:extension/types': {},
    }
  }

  return {
    // Pre-load an extension component so the synchronous core path can use it.
    async preload(name, bytes) {
      const pending = {
        scalars: [], tables: [], aggregates: [], macros: [],
        replacementScans: [], logicalTypes: [], casts: [],
      }
      const polyfill = configurePolyfill()
      const bindgen = createRuntimeBindgen({
        polyfill,
        additionalImports: buildExtensionImports(pending),
        // JSPI (nested): socket-using extensions (dns, http) block the guest on
        // async I/O (DoH fetch / ws-gateway WebSocket) via wasi:io/poll while a
        // scalar callback runs. Promote the blocking poll imports to suspending
        // and the scalar dispatch exports to promising, so the extension stack
        // can suspend. The core suspends on its `call-scalar-batch` import (see
        // run-core.mjs) and the JS bridge (coreImports) awaits these promises,
        // threading one continuous async chain core -> bridge -> extension.
        jcoOptions: {
          asyncMode: 'jspi',
          asyncImports: [
            'wasi:io/poll@0.2.6#[method]pollable.block',
            'wasi:io/poll@0.2.6#poll',
            // The http extension blocks on raw-TCP I/O directly via the stream's
            // blocking-* methods (not just poll). With the ws-gateway `tunneled`
            // tcp impl these are async (await a WebSocket round-trip), so promote
            // them to suspending too — otherwise the sync import boundary gets a
            // Promise where it expects a value and the guest traps.
            // The http extension's client (std TcpStream + read_to_end) drains
            // with the non-blocking `read` in a busy loop, treating empty as
            // "retry", never re-arming a poll. Over the ws-gateway tunnel that
            // spin starves the event loop so the next WebSocket frame / the EOF
            // never arrives. The tunneled `read` impl suspends when its buffer
            // is empty-but-not-closed (await more data), so promote `read` too.
            'wasi:io/streams@0.2.6#[method]input-stream.read',
            'wasi:io/streams@0.2.6#[method]input-stream.blocking-read',
            'wasi:io/streams@0.2.6#[method]output-stream.blocking-write-and-flush',
            'wasi:io/streams@0.2.6#[method]output-stream.blocking-flush',
          ],
          asyncExports: [
            'duckdb:extension/callback-dispatch#call-scalar',
            'duckdb:extension/callback-dispatch#call-scalar-batch',
          ],
        },
      })
      const inst = await bindgen.instantiate(bytes)
      const ext = inst.exports ?? inst
      ext.guest.load() // runs registrations into `pending`
      loaded.set(name, { instance: ext, pending })
    },

    // Imports the CORE component needs (pass via its additionalImports).
    coreImports() {
      const dispatch = (method) => (handle, ...rest) => {
        for (const { instance } of loaded.values()) {
          // single extension in this demo; route to it
          return instance.callbackDispatch[method](handle, ...rest)
        }
        throw new Error('no extension loaded for callback ' + handle)
      }
      return {
        // The core also imports the rich-types host interfaces (collation /
        // pragma / storage / index / files). The sample extension registers
        // none of them, so the empty stubs report "nothing registered"; an
        // extension that does would override these here.
        ...hostProviderStubs(),
        'duckdb:component/host-extension-loader': {
          requestLoad: (name) => loaded.has(name),
        },
        'duckdb:component/extension-loader-hooks': {
          getPendingRegistrations: () => {
            if (drained) return emptyPending()
            drained = true
            const all = emptyPending()
            for (const { pending } of loaded.values()) {
              for (const k of Object.keys(all)) all[k].push(...(pending[k] ?? []))
            }
            return all
          },
        },
        'duckdb:extension/callback-dispatch': {
          callScalar: dispatch('callScalar'),
          // Phase 1a: the core→host crossing is batched (one call per chunk);
          // the extension is still invoked per row. Row i's index is base + i.
          // The extension's call-scalar export is JSPI-promised (socket-using
          // scalars suspend on async I/O), so each per-row call returns a
          // Promise; await them and return the resolved batch. This async bridge
          // is what suspends the core, which imports call-scalar-batch as a
          // suspending import (see run-core.mjs). Sequential await preserves
          // row order and connection-state ordering across the dispatch.
          callScalarBatch: async (handle, rows, ctx) => {
            const callScalar = dispatch('callScalar')
            const base = ctx.rowindex ?? 0n
            const out = []
            for (let i = 0; i < rows.length; i++) {
              out.push(
                await callScalar(handle, rows[i], {
                  rowindex: base + BigInt(i),
                  iswindow: ctx.iswindow,
                }),
              )
            }
            return out
          },
          callTable: dispatch('callTable'),
          callAggregate: dispatch('callAggregate'),
          callPragma: dispatch('callPragma'),
          callCast: dispatch('callCast'),
        },
      }
    },
  }
}

function emptyPending() {
  return {
    scalars: [], tables: [], aggregates: [], macros: [],
    replacementScans: [], logicalTypes: [], casts: [],
  }
}
