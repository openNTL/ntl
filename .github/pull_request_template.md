## What this changes

<!-- What the change does and why. If it fixes an issue, "Closes #123". -->

## Why this way

<!--
The decisions a reviewer would otherwise have to reverse-engineer: what you
ruled out, and what constraint forced the shape of this. Skip for a typo fix.
-->

## How it was verified

<!--
What you ran, and what it showed. For a bug fix, the useful form is: the
failure reproduced first, then the same check passing. Name the test that
would catch a regression.
-->

## Checks

<!-- CI runs all of these. Ticking them before pushing saves a red round-trip. -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features`
- [ ] `cargo check -p ntl-core --target wasm32-unknown-unknown --no-default-features`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`
- [ ] Touched `mcp/ntl-postgres-mcp-server`: `npx tsc --noEmit`, `npx vitest run`, `npx wrangler deploy --dry-run --env=""`
- [ ] Touched `npm/ntl-cli`: `npm test`
- [ ] Commit messages follow the conventional-commit prefixes in CONTRIBUTING.md

## Protocol impact

- [ ] No normative requirement changed
- [ ] A requirement changed, and an RFC accompanies this — see
      [the RFC process](https://openntl.org/governance/rfc-process)

<!--
Anything a second implementation would have to match is normative: the signal
format, propagation rules, the handshake, synapse lifecycle, delivery
semantics, the learning model's observable behaviour. Changing docs to match
what the code already does is a spec change too — say which direction the fix
went and why that side was the wrong one.
-->

## Security impact

- [ ] Nothing in `spec/threat-model` is weakened by this

<!--
If this changes what an attacker can do, say so here rather than leaving it to
review. Particularly: anything touching signature coverage, the handshake,
identity binding, the influence caps, dedup, or the MCP server's read-only
enforcement.

Never open a PR that is itself the disclosure of an exploitable vulnerability.
Mail security@openntl.org first — SECURITY.md has the process.
-->
