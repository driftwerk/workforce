import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

import pkg from "./package.json" with { type: "json" };

// Every dependency stays a bare import for the host's bundler to resolve, by
// prefix so `echarts/core` and `react/jsx-runtime` are caught too.
const external = [...Object.keys(pkg.dependencies), ...Object.keys(pkg.peerDependencies)];

export default defineConfig({
  plugins: [react()],
  // Vitest stubs CSS imports to "" by default; the lane palette and the chart
  // ramps are declared in styles.css and duplicated as literal hex for the
  // standalone exports and the ECharts fallbacks, and a `?raw` import of the
  // stylesheet is what lets a test hold the two to each other.
  test: { css: true },
  build: {
    lib: { entry: "src/index.ts", formats: ["es"], fileName: "index" },
    copyPublicDir: false,
    rolldownOptions: {
      external: (id) => external.some((dep) => id === dep || id.startsWith(`${dep}/`)),
      output: { chunkFileNames: "[name]-[hash].js" },
    },
  },
});
