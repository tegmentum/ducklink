// Scenario-3 driver: run every extension's smoke.sql through the in-browser
// DuckDB and report the coverage matrix. Each extension is driven as its OWN
// page navigation (`index-corpus.html?ext=<name>`) with a per-extension timeout,
// so one hanging extension (e.g. a virtual socket that never returns) costs only
// that extension. Results merge into .corpus-results.json after each one, so the
// run is resumable across several sub-10-min invocations.
//
//   node corpus-verify.mjs                 # resume until done (or budget hit)
//   CORPUS_RESET=1 node corpus-verify.mjs  # start fresh
//   CORPUS_ONLY=isin,luhn node corpus-verify.mjs
import { createServer } from 'vite'
import { chromium } from 'playwright'
import { readFileSync, writeFileSync, existsSync, readdirSync, copyFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve, join } from 'node:path'

const WEB = dirname(fileURLToPath(import.meta.url))
const ROOT = resolve(WEB, '..')
const EXT_DIR = join(ROOT, 'extensions')
const ART_DIR = join(ROOT, 'artifacts', 'extensions')
const MERGE_FILE = join(WEB, '.corpus-results.json')
const only = (process.env.CORPUS_ONLY || '').split(',').map((s) => s.trim()).filter(Boolean)
const PER_EXT = Number(process.env.CORPUS_EXT_TIMEOUT_MS || 90000)
const BUDGET = Number(process.env.CORPUS_BUDGET_MS || 510000)

// --- manifest ---------------------------------------------------------------
const manifest = []
for (const dirent of readdirSync(EXT_DIR, { withFileTypes: true })) {
  if (!dirent.isDirectory() || !dirent.name.endsWith('-component')) continue
  const name = dirent.name.replace(/-component$/, '')
  if (only.length && !only.includes(name)) continue
  const compDir = join(EXT_DIR, dirent.name)
  const smokeSqlPath = join(compDir, 'smoke.sql')
  const wasmPath = join(ART_DIR, `${name}.wasm`)
  if (!existsSync(smokeSqlPath) || !existsSync(wasmPath)) continue
  const expectedPath = join(compDir, 'smoke.expected')
  manifest.push({
    name,
    smokeSql: readFileSync(smokeSqlPath, 'utf8'),
    smokeExpected: existsSync(expectedPath) ? readFileSync(expectedPath, 'utf8') : null,
    wasmUrl: '/@fs' + wasmPath,
  })
}
manifest.sort((a, b) => a.name.localeCompare(b.name))
writeFileSync(join(WEB, 'public', 'corpus-manifest.json'), JSON.stringify(manifest))

if (process.env.CORPUS_RESET) writeFileSync(MERGE_FILE, '{}')
let merged = {}
try { merged = JSON.parse(readFileSync(MERGE_FILE, 'utf8')) } catch {}

const todo = manifest.filter((m) => !merged[m.name])
console.log(`corpus: ${manifest.length} total, ${manifest.length - todo.length} done, ${todo.length} to run (per-ext ${PER_EXT}ms, budget ${BUDGET}ms)`)

// stage core wasm
const corePublic = join(WEB, 'public', 'ducklink_core.wasm')
if (!existsSync(corePublic)) {
  const built = resolve(ROOT, '..', 'duckdb-wasm', 'target', 'wasm32-wasip2', 'release', 'ducklink_core.wasm')
  if (existsSync(built)) copyFileSync(built, corePublic)
  else throw new Error(`core wasm missing -> ${corePublic} (cd ../duckdb-wasm && make core-browser)`)
}

// --- serve + drive per-extension -------------------------------------------
const server = await createServer({
  root: '.', logLevel: 'warn',
  optimizeDeps: { exclude: ['@tegmentum/wasi-polyfill', '@bytecodealliance/jco'] },
  server: { port: 5189, fs: { strict: false } },
})
await server.listen()
const base = server.resolvedUrls?.local?.[0] ?? 'http://localhost:5189/'
let browser = await chromium.launch()

// Run one extension. A hung/crashed renderer must not block node, so the whole
// thing races a hard timeout; on any failure the caller relaunches the browser
// (a wasm trap can crash the renderer, poisoning later pages otherwise).
async function runOne(entry) {
  const page = await browser.newPage()
  try {
    const work = (async () => {
      await page.goto(new URL(`/index-corpus.html?ext=${encodeURIComponent(entry.name)}`, base).href, { waitUntil: 'load', timeout: PER_EXT })
      await page.waitForFunction(
        () => { const el = document.getElementById('out'); return el && (el.dataset.status === 'ok' || el.dataset.status === 'error') },
        { timeout: PER_EXT },
      )
      return JSON.parse(await page.$eval('#out', (el) => el.textContent))
    })()
    return await Promise.race([
      work,
      new Promise((_, rej) => setTimeout(() => rej(new Error('hard-timeout')), PER_EXT + 15000)),
    ])
  } finally {
    page.close().catch(() => {})
  }
}

const started = Date.now()
let ranThisInvocation = 0
for (const entry of todo) {
  if (Date.now() - started > BUDGET) {
    console.log(`\n[budget] stopping after ${ranThisInvocation}; ${todo.length - ranThisInvocation} left (re-run to resume)`)
    break
  }
  let r
  try {
    r = await runOne(entry)
  } catch (e) {
    r = { status: 'ERROR', note: 'timeout/crash: ' + String(e).split('\n')[0].slice(0, 120) }
    // Relaunch the browser in case the renderer crashed.
    try { await browser.close() } catch {}
    browser = await chromium.launch()
  }
  r.name = entry.name
  merged[entry.name] = r
  writeFileSync(MERGE_FILE, JSON.stringify(merged)) // persist after each (resumable)
  ranThisInvocation++
  process.stdout.write(`${r.status === 'PASS' ? '.' : '[' + r.status[0] + ':' + entry.name + ']'}`)
}
process.stdout.write('\n')
await browser.close().catch(() => {})
await server.close()

// --- report -----------------------------------------------------------------
const fullNames = manifest.map((m) => m.name)
const results = fullNames.map((n) => merged[n]).filter(Boolean)
const by = (s) => results.filter((r) => r.status === s)
const pass = by('PASS'), mismatch = by('MISMATCH'), error = by('ERROR')
const notRun = fullNames.filter((n) => !merged[n])

console.log('\n=== Scenario-3 (browser) corpus [cumulative] ===')
console.log(`PASS ${pass.length}   MISMATCH ${mismatch.length}   ERROR ${error.length}   NOT-RUN ${notRun.length}   (of ${fullNames.length})`)
if (mismatch.length) { console.log('\n-- MISMATCH --'); for (const r of mismatch) console.log(`  ${r.name}: ${r.note}`) }
if (error.length) { console.log('\n-- ERROR --'); for (const r of error) console.log(`  ${r.name}: ${r.note}`) }
if (notRun.length) console.log('\n-- NOT-RUN --\n  ' + notRun.join(', '))

const baseline = ['isin', 'luhn', 'slug', 'rot13'].filter((n) => fullNames.includes(n))
const passNames = new Set(pass.map((r) => r.name))
const baseFail = baseline.filter((n) => !passNames.has(n))
console.log(`\nbaseline ${baseline.join(',')}: ${baseFail.length ? 'FAIL (' + baseFail.join(',') + ')' : 'ok'}`)
process.exit(notRun.length === 0 && baseFail.length === 0 ? 0 : notRun.length ? 2 : 1)
