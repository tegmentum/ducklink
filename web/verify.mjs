// Headless verification: serve index.html with Vite (native ESM, jco excluded
// from dep pre-bundling so it isn't run through esbuild), drive it with
// headless Chromium, and report the in-browser query result.
import { createServer } from 'vite'
import { chromium } from 'playwright'

const server = await createServer({
  root: '.',
  logLevel: 'warn',
  optimizeDeps: { exclude: ['@tegmentum/wasi-polyfill', '@bytecodealliance/jco'] },
  server: { port: 5188, fs: { strict: false } },
})
await server.listen()
const page_path = process.argv[2] ?? '/'
const base = server.resolvedUrls?.local?.[0] ?? 'http://localhost:5188/'
const url = new URL(page_path, base).href
console.log('serving at', url)

const browser = await chromium.launch()
const page = await browser.newPage()
page.on('console', (m) => console.log('[browser]', m.text()))
page.on('pageerror', (e) => console.log('[pageerror]', e.message))

let status = 'timeout'
let text = ''
try {
  await page.goto(url, { waitUntil: 'load' })
  // NOTE: `page.waitForFunction(fn, arg, options)` — the object below is
  // `arg` (passed into `fn`), not `options`. Passing `null` for arg makes the
  // third-slot options work; without it, playwright falls back to the 30s
  // default and long cold-start boots (44 MB wasm compile) time out.
  await page.waitForFunction(
    () => {
      const el = document.getElementById('out')
      return el && (el.dataset.status === 'ok' || el.dataset.status === 'error')
    },
    null,
    { timeout: 240000 },
  )
  const res = await page.$eval('#out', (el) => ({
    status: el.dataset.status,
    text: el.textContent,
  }))
  status = res.status
  text = res.text
} catch (e) {
  text = String(e)
}

console.log('=== RESULT status:', status, '===')
console.log(text)

await browser.close()
await server.close()
process.exit(status === 'ok' ? 0 : 1)
