import { defineConfig } from "vite";

// Cross-origin isolation (SharedArrayBuffer for the audio ring buffer); the
// production server sends the same headers.
const isolation = {
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Embedder-Policy": "require-corp",
};

export default defineConfig({
  server: {
    headers: isolation,
    // `pnpm dev` proxies the API to a locally running server.
    proxy: { "/api": "http://127.0.0.1:8080", "/auth": "http://127.0.0.1:8080" },
  },
  preview: { headers: isolation },
  worker: { format: "es" },
  build: { target: "es2024", assetsInlineLimit: 0 },
});
