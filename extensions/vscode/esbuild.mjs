// Bundles the extension (Node, CommonJS) and the webview scripts (browser, IIFE).
import * as esbuild from "esbuild"

const watch = process.argv.includes("--watch")
const common = { bundle: true, sourcemap: true, minify: !watch, logLevel: "info" }

const contexts = await Promise.all([
  esbuild.context({
    ...common,
    entryPoints: ["src/extension.ts"],
    outfile: "dist/extension.js",
    platform: "node",
    format: "cjs",
    target: "node20",
    external: ["vscode"],
  }),
  esbuild.context({
    ...common,
    entryPoints: { results: "src/webview/results.ts", lattice: "src/webview/lattice.ts" },
    outdir: "dist/webview",
    platform: "browser",
    format: "iife",
    target: "es2022",
  }),
  // Pure helpers for the unit tests in test/ (not packaged).
  esbuild.context({
    ...common,
    minify: false,
    sourcemap: false,
    entryPoints: { lib: "src/lib.ts", regions: "src/regions.ts", format: "src/shared/format.ts" },
    outdir: "out",
    outExtension: { ".js": ".mjs" },
    platform: "node",
    format: "esm",
    target: "node20",
  }),
])

if (watch) {
  await Promise.all(contexts.map((c) => c.watch()))
} else {
  await Promise.all(contexts.map((c) => c.rebuild()))
  await Promise.all(contexts.map((c) => c.dispose()))
}
