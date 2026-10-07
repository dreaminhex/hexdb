import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from "path"
import svgr from 'vite-plugin-svgr'
import tailwindcss from "@tailwindcss/vite"

// The HexDB API used by `npm run dev`. Override with HEXDB_API=http://host:port.
const api = process.env.HEXDB_API ?? "http://127.0.0.1:7700"

export default defineConfig({
  // Served by the HexDB API under /ui/ (see UI_PREFIX in hexdb_api/src/routes.rs).
  base: "/ui/",
  plugins: [react(), svgr(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    // Forward API calls to a running HexDB server during development.
    proxy: Object.fromEntries(
      ["/graphql", "/health", "/status", "/tessellations", "/users", "/roles", "/flush"].map((p) => [p, api]),
    ),
  },
})
