import { defineConfig } from 'vite'

// `@tegmentum/wasmos-browser` bundles its own runtime-guest core-module wasm
// blob (imported via `?url`) and needs to be served as native ESM so vite's
// esbuild pre-bundler doesn't inline the wasm URL export. `@wasmos/runtime-
// guest-bridge` dynamically imports @tegmentum/wasi-polyfill's plugin
// sub-paths at boot; keeping the whole chain out of pre-bundling avoids
// static-resolution errors on the polyfill's internals.
// `server.fs.strict: false` lets the vite dev server serve files from the
// sibling checkouts the two file: deps point at.
export default defineConfig({
  optimizeDeps: {
    exclude: [
      '@tegmentum/wasmos-browser',
      '@wasmos/runtime-guest-bridge',
      '@tegmentum/wasi-polyfill',
    ],
  },
  server: { fs: { strict: false } },
})
