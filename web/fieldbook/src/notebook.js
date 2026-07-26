// <fieldbook-notebook> — top-level notebook UI. Holds the ordered list of
// cells as reactive state; delegates cell rendering to <fieldbook-cell>.
//
// Talks to the fieldbook-api module for backing-store operations: cells are
// mirrored into `__fieldbook_entries` on add / edit / delete so a
// downloaded .duckdb preserves the notebook layout.
import { LitElement, html, css } from 'lit'
import './cell.js'
import {
  bootstrapSchema, ensureFieldbook, listEntries, addEntry, updateEntry,
  deleteEntry, runCellSql, recordRun, nextRunId,
} from './fieldbook-api.js'
import { snapshotFileBytes, DB_PATH } from './db.js'

// Default cell contents shown on first boot. Two starter cells give the
// user something to hit Run on immediately, and demonstrate both the
// scalar-column shape (SELECT 42) and the tabular shape (range).
const STARTER_CELLS = [
  'SELECT 42 AS answer',
  "SELECT range AS n, (range * range)::BIGINT AS n_squared FROM range(1, 6)",
]

let _nextCellId = 1

export class FieldbookNotebook extends LitElement {
  static properties = {
    cells: { attribute: false },
    ready: { type: Boolean },
    fieldbookName: { type: String, attribute: 'fieldbook-name' },
  }

  constructor() {
    super()
    this.cells = []
    this.ready = false
    this.fieldbookName = 'demo'
    this._db = null
    this._conn = null
  }

  static styles = css`
    :host { display: block; }
    .empty {
      color: var(--muted, #666);
      font-style: italic;
      padding: 2rem;
      text-align: center;
      border: 1px dashed var(--border, #ddd);
      border-radius: 6px;
    }
  `

  // Called by main.js after the core is instantiated. Bootstraps the
  // fieldbook schema, seeds the starter cells if the notebook is empty,
  // and mounts the UI.
  async attachDb(db, conn) {
    this._db = db
    this._conn = conn
    await bootstrapSchema(db, conn)
    await ensureFieldbook(db, conn, this.fieldbookName)
    const existing = await listEntries(db, conn, this.fieldbookName)
    if (existing.length === 0) {
      // First run: seed the starter cells directly into the backing table
      // so the notebook state is round-trippable through a downloaded
      // .duckdb from the very first session.
      for (const src of STARTER_CELLS) {
        await addEntry(db, conn, this.fieldbookName, src)
      }
    }
    // Load whatever's in the backing table (either the seed or a
    // rehydrated set) into the reactive `cells` array.
    const rows = await listEntries(db, conn, this.fieldbookName)
    this.cells = rows.map((r) => ({
      id: _nextCellId++,
      ordinal: Number(r.ordinal),
      source: r.source,
      status: 'idle',
      result: null,
      error: null,
      durationMs: 0,
    }))
    this.ready = true
    this.requestUpdate()
  }

  render() {
    return html`
      <div class="toolbar" style="
        display:flex; gap:0.5rem; align-items:center;
        padding: 0.5rem 0; margin-bottom: 0.5rem;
      ">
        <strong>Fieldbook</strong>
        <span style="color:var(--muted,#666); font-family:ui-monospace,monospace;">
          — ${this.fieldbookName}
        </span>
        <span style="flex:1"></span>
        <button @click=${this._onNewCell} ?disabled=${!this.ready}>
          + New cell
        </button>
        <button @click=${this._onRunAll} ?disabled=${!this.ready || this.cells.length === 0}>
          Run all
        </button>
        <button
          @click=${this._onDownload}
          ?disabled=${!this.ready}
          title="Save the current in-memory DuckDB as fieldbook.duckdb"
        >Download .duckdb</button>
      </div>
      ${!this.ready
        ? html`<div class="empty">initialising DuckDB core…</div>`
        : this.cells.length === 0
          ? html`
            <div class="empty">
              No cells. Hit "+ New cell" to add one.
            </div>`
          : this.cells.map(
              (c, i) => html`
                <fieldbook-cell
                  .cell=${c}
                  .index=${i}
                  @cell-run=${(e) => this._runCell(e.detail.id)}
                  @cell-delete=${(e) => this._deleteCell(e.detail.id)}
                  @cell-edit=${(e) => this._editCell(e.detail.id, e.detail.source)}
                ></fieldbook-cell>`,
            )}
    `
  }

  async _onNewCell() {
    if (!this._db || !this._conn) return
    const ord = await addEntry(this._db, this._conn, this.fieldbookName, '')
    this.cells = [
      ...this.cells,
      {
        id: _nextCellId++,
        ordinal: Number(ord),
        source: '',
        status: 'idle',
        result: null,
        error: null,
        durationMs: 0,
      },
    ]
  }

  async _onRunAll() {
    if (!this._db || !this._conn) return
    for (const c of [...this.cells]) {
      await this._runCellById(c.id)
    }
  }

  _runCell(id) { return this._runCellById(id) }

  async _runCellById(id) {
    const idx = this.cells.findIndex((c) => c.id === id)
    if (idx < 0) return
    const cell = { ...this.cells[idx], status: 'running', error: null }
    this._patchCell(idx, cell)
    const runId = await nextRunId(this._db, this._conn)
    const outcome = await runCellSql(this._db, this._conn, cell.source)
    const next = {
      ...cell,
      status: outcome.error ? 'error' : 'ok',
      result: outcome.result,
      error: outcome.error,
      durationMs: outcome.durationMs,
    }
    this._patchCell(idx, next)
    const rowCount = outcome.result && outcome.result.rows
      ? outcome.result.rows.length : -1
    await recordRun(
      this._db, this._conn, this.fieldbookName, cell.ordinal, runId,
      outcome.durationMs, outcome.error ? 'error' : 'ok',
      outcome.error || '', rowCount,
    )
  }

  async _deleteCell(id) {
    const idx = this.cells.findIndex((c) => c.id === id)
    if (idx < 0) return
    const cell = this.cells[idx]
    await deleteEntry(this._db, this._conn, this.fieldbookName, cell.ordinal)
    this.cells = this.cells.filter((c) => c.id !== id)
  }

  async _editCell(id, source) {
    const idx = this.cells.findIndex((c) => c.id === id)
    if (idx < 0) return
    const cell = { ...this.cells[idx], source }
    this._patchCell(idx, cell)
    // Debounce persist: keep it simple — write on every input. The volume
    // is tiny and DuckDB handles it comfortably.
    await updateEntry(
      this._db, this._conn, this.fieldbookName, cell.ordinal, source,
    )
  }

  async _onDownload() {
    if (!this._db || !this._conn) return
    // Flush the WAL so the file on the memfs represents committed state.
    try { await this._db.execute(this._conn, 'CHECKPOINT') } catch { /* fine */ }
    const bytes = snapshotFileBytes(DB_PATH)
    if (!bytes) {
      alert(
        'No database file to download. The DB is opened in-memory only; ' +
        'this build should open a file-backed database at ' + DB_PATH + '.',
      )
      return
    }
    const blob = new Blob([bytes], { type: 'application/octet-stream' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = 'fieldbook.duckdb'
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
    setTimeout(() => URL.revokeObjectURL(url), 1000)
  }

  _patchCell(idx, next) {
    this.cells = [
      ...this.cells.slice(0, idx),
      next,
      ...this.cells.slice(idx + 1),
    ]
  }
}

customElements.define('fieldbook-notebook', FieldbookNotebook)
