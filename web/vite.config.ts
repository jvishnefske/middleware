import { defineConfig } from "vitest/config";
import { fileURLToPath } from "node:url";

// The repository root (`middleware/`), so the editor can glob the payload
// templates in ../payloads/templates from the dev server.
const repoRoot = fileURLToPath(new URL("..", import.meta.url));

export default defineConfig({
  base: "./",
  server: {
    // Web Serial needs a secure context; localhost qualifies.
    host: "localhost",
    fs: { allow: [repoRoot] },
  },
  build: {
    // Top-level await (binaryen inside the AssemblyScript compiler).
    target: "esnext",
    chunkSizeWarningLimit: 16000,
  },
  optimizeDeps: {
    exclude: ["assemblyscript"],
  },
  test: {
    environment: "node",
    include: ["test/**/*.test.ts"],
    testTimeout: 20000,
  },
});
