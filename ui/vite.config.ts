import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { sri } from "vite-plugin-sri3";
import mdx from "fumadocs-mdx/vite";
import * as sourceConfig from "./source.config";
import path from "path";

// SRI injects integrity="sha384-..." into every script/link in
// index.html at build time so the browser refuses to execute a
// tampered bundle. Closes audit finding UI-F4 (2026-05-15).
export default defineConfig(async () => ({
  plugins: [await mdx(sourceConfig), react(), tailwindcss(), sri()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      "collections": path.resolve(__dirname, "./.source"),
    },
  },
  build: { outDir: "dist", sourcemap: false },
  server: {
    proxy: {
      "/api": { target: "http://localhost:8091", changeOrigin: true },
      "/ws": { target: "ws://localhost:8091", ws: true },
    },
  },
}));
