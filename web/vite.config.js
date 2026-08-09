import { defineConfig } from 'vite'

// `@tegmentum/wasi-polyfill` internally imports the browser build of
// `@bytecodealliance/jco/component` (used to unbundle the wasip2 component
// into core modules) at RUNTIME. jco's `js-component-bindgen-component.js`
// has a runtime-only `const offset = …; offset = …` reassignment that
// esbuild's pre-bundler rejects statically; serving both packages as
// native ESM keeps that branch out of the pre-bundler. `fs.strict: false`
// lets the browser fetch jco's linked `.core.wasm` helpers when
// wasi-polyfill is symlinked in.
export default defineConfig({
  optimizeDeps: { exclude: ['@tegmentum/wasi-polyfill', '@bytecodealliance/jco'] },
  server: { fs: { strict: false } },
})
