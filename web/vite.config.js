import { defineConfig } from 'vite'

// jco's browser build and wasi-polyfill must be served as native ESM (jco's
// generated bindgen has a runtime-only `const` reassignment that esbuild's
// pre-bundler rejects statically). `fs.strict: false` lets jco load its own
// `.core.wasm` helpers from the linked wasi-polyfill checkout.
export default defineConfig({
  optimizeDeps: { exclude: ['@tegmentum/wasi-polyfill', '@bytecodealliance/jco'] },
  server: { fs: { strict: false } },
})
