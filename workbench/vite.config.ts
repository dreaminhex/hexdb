import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"

// Tauri serves the built files; in development it loads this server.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5174, strictPort: true },
  build: { target: "es2022" },
})
