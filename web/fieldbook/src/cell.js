// <fieldbook-cell> — one notebook cell: SQL source area + Run button +
// output pane. Purely presentational; state lives on <fieldbook-notebook>.
//
// Events dispatched (bubbling, composed):
//   - `cell-run`    { detail: { id } }  — user hit the Run button or Cmd+Enter
//   - `cell-delete` { detail: { id } }  — user hit the Delete button
//   - `cell-edit`   { detail: { id, source } } — source textarea changed
import { LitElement, html, css } from 'lit'

export class FieldbookCell extends LitElement {
  static properties = {
    cell: { attribute: false },
    index: { type: Number },
  }

  static styles = css`
    :host {
      display: block;
      border: 1px solid var(--border, #ddd);
      border-radius: 6px;
      margin-bottom: 0.75rem;
      background: white;
      overflow: hidden;
    }
    .cell-header {
      display: flex;
      gap: 0.5rem;
      align-items: center;
      background: var(--bg-alt, #f7f7f9);
      border-bottom: 1px solid var(--border, #ddd);
      padding: 0.35rem 0.6rem;
      font-family: ui-monospace, monospace;
      font-size: 12px;
      color: var(--muted, #666);
    }
    .cell-header .ord { font-weight: 600; color: #333; }
    .cell-header .spacer { flex: 1; }
    .cell-header button {
      border: 1px solid var(--border, #ddd);
      background: white;
      padding: 0.15rem 0.6rem;
      border-radius: 3px;
      font: inherit;
      cursor: pointer;
    }
    .cell-header button.run {
      background: var(--accent, #2b5edd);
      color: white;
      border-color: var(--accent, #2b5edd);
    }
    .cell-header button:disabled { opacity: 0.5; cursor: wait; }
    textarea {
      display: block;
      width: 100%;
      min-height: 3rem;
      max-height: 20rem;
      box-sizing: border-box;
      border: 0;
      resize: vertical;
      padding: 0.5rem 0.7rem;
      font: 13px/1.4 ui-monospace, monospace;
      background: white;
      color: var(--fg, #111);
      outline: none;
    }
    .status {
      padding: 0.2rem 0.7rem;
      font-family: ui-monospace, monospace;
      font-size: 11px;
      color: var(--muted, #666);
      border-top: 1px solid var(--border, #ddd);
      background: var(--bg-alt, #f7f7f9);
    }
    .status.ok    { color: var(--ok, #060); }
    .status.error { color: var(--error, #a00); }
    .output {
      max-height: 320px;
      overflow: auto;
      padding: 0.5rem 0.7rem;
    }
    .output .err {
      color: var(--error, #a00);
      white-space: pre-wrap;
      font-family: ui-monospace, monospace;
      font-size: 12px;
    }
    .output .empty {
      color: var(--muted, #666);
      font-style: italic;
      font-size: 12px;
    }
    table {
      border-collapse: collapse;
      font-size: 12px;
      width: 100%;
    }
    th, td {
      border: 1px solid #e0e0e0;
      padding: 3px 8px;
      text-align: left;
      max-width: 260px;
      overflow: hidden;
      white-space: nowrap;
      text-overflow: ellipsis;
    }
    th { background: #f4f4f4; font-weight: 600; }
    tbody tr:nth-child(even) { background: #fafafa; }
    td.num { text-align: right; font-variant-numeric: tabular-nums; }
  `

  render() {
    const cell = this.cell || {}
    const status = cell.status || 'idle'
    const rows = cell.result ? cell.result.rows : null
    const columns = cell.result ? cell.result.columns : null
    return html`
      <div class="cell-header">
        <span class="ord">[${this.index + 1}]</span>
        <span>ord ${cell.ordinal ?? '?'}</span>
        <span class="spacer"></span>
        <button
          class="run"
          @click=${this._onRun}
          ?disabled=${status === 'running'}
          title="Run (Cmd/Ctrl-Enter)"
        >Run</button>
        <button @click=${this._onDelete} title="Delete cell">Delete</button>
      </div>
      <textarea
        .value=${cell.source ?? ''}
        @input=${this._onInput}
        @keydown=${this._onKeydown}
        spellcheck="false"
        placeholder="SELECT 1"
      ></textarea>
      <div class="status ${status === 'error' ? 'error' : status === 'ok' ? 'ok' : ''}">
        ${this._statusText(cell)}
      </div>
      <div class="output">
        ${cell.error
          ? html`<pre class="err">${cell.error}</pre>`
          : cell.result
            ? this._renderTable(columns, rows)
            : html`<span class="empty">(no output — hit Run)</span>`}
      </div>
    `
  }

  _statusText(cell) {
    if (cell.status === 'running') return 'running…'
    if (cell.status === 'error')
      return `error (${cell.durationMs ?? 0} ms)`
    if (cell.status === 'ok') {
      const n = cell.result && cell.result.rows ? cell.result.rows.length : 0
      return `ok — ${n} row${n === 1 ? '' : 's'} in ${cell.durationMs ?? 0} ms`
    }
    return 'idle'
  }

  _renderTable(columns, rows) {
    if (!columns || !rows) return html`<span class="empty">(no output)</span>`
    if (rows.length === 0) return html`<span class="empty">(0 rows)</span>`
    const headerCells = columns.map(
      (c) => html`<th>${c.name}</th>`,
    )
    const bodyRows = rows.slice(0, 500).map(
      (r) => html`<tr>${r.map((v, i) => this._renderCell(v, columns[i]))}</tr>`,
    )
    const truncated = rows.length > 500
      ? html`<div class="empty">(showing first 500 of ${rows.length} rows)</div>`
      : null
    return html`
      <table>
        <thead><tr>${headerCells}</tr></thead>
        <tbody>${bodyRows}</tbody>
      </table>
      ${truncated}
    `
  }

  _renderCell(v, col) {
    if (v === null || v === undefined) {
      return html`<td class="null" style="color:#999">NULL</td>`
    }
    const isNumeric =
      typeof v === 'number' || typeof v === 'bigint'
    const text =
      typeof v === 'bigint' ? v.toString() + 'n' : String(v)
    return html`<td class="${isNumeric ? 'num' : ''}" title="${text}">${text}</td>`
  }

  _onRun() {
    this.dispatchEvent(
      new CustomEvent('cell-run', {
        bubbles: true, composed: true, detail: { id: this.cell.id },
      }),
    )
  }

  _onDelete() {
    this.dispatchEvent(
      new CustomEvent('cell-delete', {
        bubbles: true, composed: true, detail: { id: this.cell.id },
      }),
    )
  }

  _onInput(e) {
    this.dispatchEvent(
      new CustomEvent('cell-edit', {
        bubbles: true, composed: true,
        detail: { id: this.cell.id, source: e.target.value },
      }),
    )
  }

  _onKeydown(e) {
    // Cmd/Ctrl-Enter runs the cell (Shift-Enter and plain Enter add newlines).
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault()
      this._onRun()
    }
  }
}

customElements.define('fieldbook-cell', FieldbookCell)
