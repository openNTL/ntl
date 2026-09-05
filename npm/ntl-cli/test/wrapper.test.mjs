/**
 * Tests for the npm wrapper and its installer.
 *
 * The wrapper is thin, but every path through it is one a user hits on a bad
 * day — a failed download, an unsupported platform, a non-zero exit from the
 * binary. Those are exactly the paths worth testing.
 */

import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const WRAPPER = join(ROOT, "bin", "ntl.mjs");
const BINARY = join(ROOT, "bin", process.platform === "win32" ? "ntl.exe" : "ntl");

/** Run the wrapper, capturing everything. */
function runWrapper(args = []) {
  return spawnSync(process.execPath, [WRAPPER, ...args], { encoding: "utf8" });
}

test("explains itself when the binary is absent", () => {
  rmSync(BINARY, { force: true });
  const result = runWrapper(["--version"]);

  assert.equal(result.status, 1, "should exit non-zero");
  // A bare "not found" leaves the user stuck. The message must name a fix.
  assert.match(result.stderr, /binary is not present/);
  assert.match(result.stderr, /npm install --force/);
  assert.match(result.stderr, /cargo install/);
});

test("delegates to the binary and passes through its exit code", () => {
  // A stub standing in for the Rust binary, so this test needs no build.
  const stub =
    process.platform === "win32"
      ? null
      : "#!/bin/sh\nif [ \"$1\" = \"fail\" ]; then echo 'oh no' >&2; exit 7; fi\necho \"args: $*\"\nexit 0\n";

  if (!stub) return; // Windows shell stub is not worth the complexity here.

  writeFileSync(BINARY, stub);
  chmodSync(BINARY, 0o755);

  try {
    const ok = runWrapper(["status", "--home", "/tmp/x"]);
    assert.equal(ok.status, 0);
    assert.match(ok.stdout, /args: status --home \/tmp\/x/);

    const failed = runWrapper(["fail"]);
    assert.equal(failed.status, 7, "the binary's exit code must survive");
    assert.match(failed.stderr, /oh no/);
  } finally {
    rmSync(BINARY, { force: true });
  }
});

test("installer exits cleanly when downloads are skipped", () => {
  // A postinstall that fails breaks `npm install` for the whole project, so it
  // must never exit non-zero.
  const result = spawnSync(
    process.execPath,
    [join(ROOT, "scripts", "install.mjs")],
    { encoding: "utf8", env: { ...process.env, NTL_SKIP_DOWNLOAD: "1" } },
  );
  assert.equal(result.status, 0);
  assert.match(result.stderr, /skipping binary download/);
});

test("installer survives an unreachable release host without failing install", () => {
  // Point the download at a closed local port, which refuses immediately: no
  // DNS, no network, no dependence on whether a release happens to exist.
  //
  // This previously set HTTPS_PROXY/HTTP_PROXY to a dead port and claimed to
  // "force resolution failure without touching the network". That never
  // worked: Node's global fetch ignores those variables unless given an
  // explicit dispatcher, so the request went to the real GitHub. The test
  // passed only because no release existed yet and the fetch 404'd — and it
  // started failing the moment v0.2.0-beta.1 was published and the download
  // began succeeding.
  const home = mkdtempSync(join(tmpdir(), "ntl-npm-"));
  try {
    const result = spawnSync(
      process.execPath,
      [join(ROOT, "scripts", "install.mjs")],
      {
        encoding: "utf8",
        env: {
          ...process.env,
          NTL_SKIP_DOWNLOAD: "",
          NTL_RELEASE_BASE_URL: "http://127.0.0.1:1",
          npm_config_cache: home,
        },
      },
    );
    assert.equal(
      result.status,
      0,
      "a failed download must not break `npm install`",
    );
    assert.match(result.stderr, /cargo install|no prebuilt binary|could not download/);
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
});

test("package metadata is coherent", () => {
  const pkg = JSON.parse(
    execFileSync(process.execPath, [
      "-e",
      `process.stdout.write(require('fs').readFileSync('${join(ROOT, "package.json")}','utf8'))`,
    ]).toString(),
  );

  assert.equal(pkg.name, "@bundu/ntl-cli");
  assert.equal(pkg.bin.ntl, "bin/ntl.mjs");
  assert.equal(pkg.license, "Apache-2.0");
  // The published tarball must carry the wrapper and installer, or the package
  // is inert.
  assert.ok(pkg.files.includes("bin/"));
  assert.ok(pkg.files.includes("scripts/"));
  assert.ok(pkg.scripts.postinstall.includes("install.mjs"));
});
