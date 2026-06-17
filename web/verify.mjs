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
const url = server.resolvedUrls?.local?.[0] ?? 'http://localhost:5188/'
console.log('serving at', url)

const browser = await chromium.launch()
const page = await browser.newPage()
page.on('console', (m) => console.log('[browser]', m.text()))
page.on('pageerror', (e) => console.log('[pageerror]', e.message))

let status = 'timeout'
let text = ''
try {
  await page.goto(url, { waitUntil: 'load' })
  await page.waitForFunction(
    () => {
      const el = document.getElementById('out')
      return el && (el.dataset.status === 'ok' || el.dataset.status === 'error')
    },
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
