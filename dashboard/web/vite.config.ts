import path from 'node:path'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// Built assets are served by nginx (dashboard/nginx.conf), which also proxies
// /api/bot and /api/recon to the WG-internal APIs. `npm run dev` can proxy to a
// live backend by setting MAJNET_API (e.g. http://majksa over the tailnet).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  // react-draggable (via react-grid-layout, the Overview "Customize" grid) does
  // `if (process.env.DRAGGABLE_DEBUG)` in its drag-start path. `process` doesn't
  // exist in the browser, so a drag threw `process is not defined` and aborted.
  // Replace the reference at build time (dev + prod) so drag/resize work.
  define: { 'process.env.DRAGGABLE_DEBUG': 'false' },
  optimizeDeps: { esbuildOptions: { define: { 'process.env.DRAGGABLE_DEBUG': 'false' } } },
  resolve: { alias: { '@': path.resolve(__dirname, './src') } },
  server: process.env.MAJNET_API
    ? { proxy: { '/api': { target: process.env.MAJNET_API, changeOrigin: true } } }
    : undefined,
})
