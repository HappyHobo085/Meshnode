import { svelte } from "@sveltejs/vite-plugin-svelte";
import { svelteTesting } from "@testing-library/svelte/vite";
import { defineConfig } from "vitest/config";

// The Tauri shells load `dist/` under a strict CSP (script-src 'self'; style-src 'self';
// img-src 'self' data:; no remote anything), so the build must emit only external files:
// no inline scripts, no modulepreload polyfill, no data: fonts.
export default defineConfig({
  plugins: [svelte(), svelteTesting()],
  publicDir: "static",
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    cssCodeSplit: false,
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./vitest.setup.ts"],
    include: ["src/**/*.test.ts"],
  },
});
