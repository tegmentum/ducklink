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
import { configurePolyfill } from './run-core.mjs'

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
        pending.tables.push({ name, arguments: args, columns, callbackHandle: cb.handle, options })
        return id()
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
        // scan.tableFunction is the handle the table register() returned; map it
        // to the table function name captured above.
        const fn = pending.tables[scan.tableFunction - pending._tableHandleBase]
        pending.replacementScans.push({
          extensions: scan.extensions,
          functionName: fn ? fn.name : '',
        })
        return id()
      },
      registerCopyHandler() {
        throw new Error('copy handlers are not supported')
      },
    }

    // Table register() returns ids starting at the running counter; remember the
    // base so register-replacement-scan can resolve handle -> table index.
    pending._tableHandleBase = nextId
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
