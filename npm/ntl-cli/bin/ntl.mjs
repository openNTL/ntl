#!/usr/bin/env node
/**
 * Wrapper that execs the platform `ntl` binary.
 *
 * Uses `spawnSync` with `stdio: "inherit"` so the binary owns the terminal:
 * openNTL's CLI prints colour, and `ntl listen` is long-running and must
 * receive ctrl-c directly rather than through a relay.
 */

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const binary = join(HERE, process.platform === "win32" ? "ntl.exe" : "ntl");

if (!existsSync(binary)) {
  process.stderr.write(
    [
      "The ntl binary is not present.",
      "",
      "This usually means the postinstall download failed — a proxy, an",
      "offline install, or a platform without a prebuilt binary.",
      "",
      "Reinstall:",
      "  npm install --force @nyuchi/ntl-cli",
      "",
      "Or build from source, which always works if you have Rust:",
      "  cargo install --git https://github.com/openNTL/ntl ntl-cli",
      "",
    ].join("\n"),
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  process.stderr.write(`failed to run ntl: ${result.error.message}\n`);
  process.exit(1);
}

// Preserve signal deaths as the conventional 128+n, so a ctrl-c out of
// `ntl listen` looks like a ctrl-c to the calling shell.
if (result.signal) {
  const signals = { SIGINT: 2, SIGTERM: 15, SIGHUP: 1, SIGQUIT: 3 };
  process.exit(128 + (signals[result.signal] ?? 0));
}

process.exit(result.status ?? 0);
