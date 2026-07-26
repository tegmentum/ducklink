// Entry point for the fieldbook browser demo. Auto-boots on DOM ready.
//
// Boot sequence:
//   1. Fetch ducklink_core.wasm bytes.
//   2. Instantiate it via jco + @tegmentum/wasi-polyfill (see db.js).
//   3. Open a file-backed database at /fieldbook.duckdb (memfs-backed, so
//      the "Download .duckdb" button can read the bytes out).
//   4. Hand (db, conn) to the <fieldbook-notebook> element which bootstraps
//      the __fieldbook_* schema, seeds starter cells, and takes over.
//
// We intentionally do NOT load fieldbook.wasm in Phase 1 — the notebook
// talks direct SQL to the __fieldbook_* tables (mirroring the dotcmd
// pattern in extensions/fieldbook-dotcmd/src/lib.rs). fieldbook.wasm is
// still shipped in dist/ so a future upgrade can wire it in without
// changing URLs. See README.md for the rationale.
import './notebook.js'
import { instantiateCore, DB_PATH } from './db.js'

async function fetchBytes(url) {
  const r = await fetch(url)
  if (!r.ok) throw new Error(`fetch ${url}: HTTP ${r.status}`)
  return new Uint8Array(await r.arrayBuffer())
}

async function boot() {
  const statusEl = document.getElementById('status')
  const notebookEl = document.getElementById('nb')
  const status = (m) => {
    if (statusEl) statusEl.textContent = m
    // eslint-disable-next-line no-console
    console.log('[fieldbook]', m)
  }
  const die = (e) => {
    if (statusEl) {
      statusEl.style.color = 'var(--error)'
      statusEl.textContent = 'boot failed: ' + (e && (e.stack || e.message) || e)
    }
    console.error('[fieldbook] boot failed:', e)
  }

  try {
    status('fetching ducklink_core.wasm…')
    const coreBytes = await fetchBytes('./ducklink_core.wasm')
    status(`instantiating core (${coreBytes.length.toLocaleString()} B)…`)
    const db = await instantiateCore(coreBytes)

    // Presence-check fieldbook.wasm so a missing artifact surfaces in the
    // status line rather than as a silent gap. Byte count is informational.
    try {
      const fbBytes = await fetchBytes('./fieldbook.wasm')
      status(
        `core ready + fieldbook.wasm staged (${fbBytes.length.toLocaleString()} B; not loaded in Phase 1)`,
      )
    } catch (e) {
      status(
        `core ready — fieldbook.wasm not found (Phase 1 uses direct SQL, OK): ${e.message || e}`,
      )
    }

    status(`opening ${DB_PATH}…`)
    const conn = db.open(DB_PATH)

    status('bootstrapping fieldbook schema…')
    await notebookEl.attachDb(db, conn)

    status(`ready — ${DB_PATH} — ${notebookEl.cells.length} cell(s)`)
  } catch (e) {
    die(e)
  }
}

// Guard against being loaded twice; auto-boot after Lit has upgraded the
// custom element (its class definition is registered by the notebook.js
// import above, so by the time this module's top-level runs the element is
// upgraded synchronously).
boot()
