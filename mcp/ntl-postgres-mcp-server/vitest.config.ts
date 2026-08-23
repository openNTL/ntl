import { readFileSync } from "node:fs";

import { defineConfig } from "vitest/config";

/**
 * Mirror Wrangler's Text rule for `.sql` imports, so tests and production load
 * the same schema file. Generating a TypeScript copy would let them drift.
 */
const sqlAsText = {
  name: "sql-as-text",
  transform(_code: string, id: string) {
    if (!id.endsWith(".sql")) return null;
    return {
      code: `export default ${JSON.stringify(readFileSync(id, "utf8"))};`,
      map: null,
    };
  },
};

export default defineConfig({
  plugins: [sqlAsText],
  test: {
    include: ["test/**/*.test.ts"],
    // PGlite boots a WebAssembly Postgres, which takes a moment on first use.
    testTimeout: 60_000,
    hookTimeout: 60_000,
    // Serial: the real-Postgres suite shares one database, and parallel
    // schema mutation would make failures depend on scheduling.
    fileParallelism: false,
  },
});
