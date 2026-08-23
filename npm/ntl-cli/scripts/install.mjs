#!/usr/bin/env node
/**
 * Fetch the `ntl` binary for this platform.
 *
 * openNTL's CLI is a Rust binary. npm is the distribution channel because it is
 * the one almost every developer already has, not because the CLI is
 * JavaScript.
 *
 * Design notes, since postinstall scripts are a common source of pain:
 *
 *  - **Failure here is not fatal.** A postinstall that exits non-zero breaks
 *    `npm install` for the whole project, which is a hostile thing for a CLI to
 *    do. If the download fails, this warns and exits 0; the wrapper then gives
 *    a clear error when the binary is actually needed.
 *  - **Checksums are verified.** A binary fetched over the network and executed
 *    without verification is an supply-chain problem waiting to happen.
 *  - **CI and offline installs are respected.** `NTL_SKIP_DOWNLOAD=1` skips
 *    entirely, and an existing binary is not re-fetched.
 */

import { createHash } from "node:crypto";
import { chmod, mkdir, readFile, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const BIN_DIR = join(ROOT, "bin");

const pkg = JSON.parse(await readFile(join(ROOT, "package.json"), "utf8"));
const VERSION = pkg.version;

/** Map Node's platform/arch onto a release asset name. */
const TARGETS = {
  "darwin-arm64": "ntl-aarch64-apple-darwin",
  "darwin-x64": "ntl-x86_64-apple-darwin",
  "linux-arm64": "ntl-aarch64-unknown-linux-gnu",
  "linux-x64": "ntl-x86_64-unknown-linux-gnu",
  "win32-x64": "ntl-x86_64-pc-windows-msvc.exe",
};

function log(message) {
  process.stderr.write(`@nyuchi/ntl-cli: ${message}\n`);
}

async function main() {
  if (process.env["NTL_SKIP_DOWNLOAD"]) {
    log("NTL_SKIP_DOWNLOAD set; skipping binary download.");
    return;
  }

  const key = `${process.platform}-${process.arch}`;
  const asset = TARGETS[key];

  if (!asset) {
    // Not an error: plenty of valid reasons to install on an unsupported
    // platform (a lockfile on a build machine, for instance).
    log(
      `no prebuilt binary for ${key}. Build from source:\n` +
        `  cargo install --git https://github.com/openNTL/ntl ntl-cli`,
    );
    return;
  }

  const target = join(BIN_DIR, process.platform === "win32" ? "ntl.exe" : "ntl");
  if (existsSync(target)) return;

  const base = `https://github.com/openNTL/ntl/releases/download/v${VERSION}`;
  await mkdir(BIN_DIR, { recursive: true });

  try {
    // Checksums first, so a tampered binary is caught before it is written.
    const sumsResponse = await fetch(`${base}/checksums.txt`);
    if (!sumsResponse.ok) {
      throw new Error(`checksums.txt returned ${sumsResponse.status}`);
    }
    const sums = await sumsResponse.text();
    const expected = sums
      .split("\n")
      .map((line) => line.trim().split(/\s+/))
      .find(([, name]) => name === asset)?.[0];

    if (!expected) {
      throw new Error(`no checksum listed for ${asset}`);
    }

    const binaryResponse = await fetch(`${base}/${asset}`);
    if (!binaryResponse.ok) {
      throw new Error(`${asset} returned ${binaryResponse.status}`);
    }
    const bytes = Buffer.from(await binaryResponse.arrayBuffer());

    const actual = createHash("sha256").update(bytes).digest("hex");
    if (actual !== expected) {
      // Refuse rather than warn: writing an unverified binary that will later
      // be executed is the one failure mode worth being loud about.
      throw new Error(
        `checksum mismatch for ${asset}\n  expected ${expected}\n  got      ${actual}`,
      );
    }

    await writeFile(target, bytes);
    if (process.platform !== "win32") await chmod(target, 0o755);
    log(`installed ntl ${VERSION} for ${key}`);
  } catch (error) {
    // Warn, do not throw. A failed postinstall would break `npm install` for
    // the entire project, which is far worse than a CLI that reports a clear
    // error when it is first run.
    log(
      `could not download the binary (${error.message}).\n` +
        `  The 'ntl' command will explain this when you run it.\n` +
        `  Build from source instead:\n` +
        `    cargo install --git https://github.com/openNTL/ntl ntl-cli`,
    );
  }
}

await main();
