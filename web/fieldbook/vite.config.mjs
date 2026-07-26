// Vite build config for the fieldbook browser demo.
//
// Vite (not esbuild-standalone) is required because @tegmentum/wasi-polyfill's
// RuntimeBindgen dynamically imports `@bytecodealliance/jco/component` at
// runtime for wasm-component transpilation. jco's generated bundle contains
// (a) a const-reassignment that trips esbuild's strict parser, and (b)
// `await import('node:fs/promises')` in browser-unreachable helpers that
// esbuild still tries to resolve at bundle time. Vite handles both — the
// dynamic import is preserved as a separately-chunked lazy module, and
// node-only branches are transparently skipped. The wasi-polyfill's own
// e2e tests use Vite for the same reason.
//
// Output layout matches what run.sh + `make fieldbook-browser` expects:
//   dist/index.html
//   dist/assets/<hashed bundle chunks>
// The two wasm artifacts (ducklink_core.wasm, fieldbook.wasm) are staged
// into dist/ AFTER `vite build` by the Makefile — Vite would try to inline
// / copy them if we put them in src/, and we want them served untouched.
import { defineConfig } from 'vite'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = fileURLToPath(new URL('.', import.meta.url))

export default defineConfig({
  root: resolve(HERE, 'src'),
  publicDir: false,
  build: {
    outDir: resolve(HERE, 'dist'),
    emptyOutDir: true,
    // Playwright / modern Chromium only — no lowering needed and it keeps
    // async/await + private-class-field bundles a lot smaller. Same target
    // wasi-polyfill's e2e vite config uses.
    target: 'esnext',
    rollupOptions: {
      input: {
        main: resolve(HERE, 'src/index.html'),
      },
    },
  },
  server: {
    port: 8789,
    strictPort: true,
  },
  preview: {
    port: 8789,
    strictPort: true,
  },
})
