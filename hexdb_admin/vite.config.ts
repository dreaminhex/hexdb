import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from "path"
import svgr from 'vite-plugin-svgr'
import tailwindcss from "@tailwindcss/vite"

export default defineConfig({
  // Served by the HexDB API under /ui/ (see UI_PREFIX in hexdb_api/src/routes.rs).
  base: "/ui/",
  plugins: [react(), svgr(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
})
