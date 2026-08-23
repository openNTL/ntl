# @nyuchi/ntl-cli

Command-line interface for [openNTL](https://openntl.org) — signal transport
that learns its routes.

```bash
npm install -g @nyuchi/ntl-cli

ntl init
ntl start --dev
```

## What this package is

The openNTL CLI is a Rust binary. This package distributes it through npm,
because npm is the package manager almost every developer already has.

On install it downloads the binary for your platform from the
[GitHub release](https://github.com/openNTL/ntl/releases) and verifies its
SHA-256 checksum. `bin/ntl.mjs` is a thin wrapper that execs it.

Prefer to build from source, or on a platform without a prebuilt binary:

```bash
cargo install --git https://github.com/openNTL/ntl ntl-cli
```

## Quick start

```bash
# Create identity, config, and a SQLite store under ~/.ntl
ntl init

# Terminal 1 — receive
ntl listen --listen 127.0.0.1:14433

# Terminal 2 — send, and wait for the receipt
ntl emit --type data --payload '{"hello":"world"}' \
         --acknowledged --peer 127.0.0.1:14433
```

The sender prints `✓ receipt delivered weight +0.0322`. That last number is the
routing model updating: the receipt came back, resolved the journalled routing
decision, and strengthened the synapse that carried it.

Full walkthrough: [openntl.org/guides/quickstart](https://openntl.org/guides/quickstart).

## Commands

| Command | Does |
|---|---|
| `ntl init` | Create identity, config and store |
| `ntl start` | Run a node (`--dev` binds loopback only) |
| `ntl emit` | Emit a signal (`--acknowledged` for at-least-once) |
| `ntl listen` | Print signals as they arrive |
| `ntl synapses` | Show learned weights and per-type affinity |
| `ntl status` | Node state and routing-model health |
| `ntl topology` | Known peers and their provenance |

## Environment

| Variable | Effect |
|---|---|
| `NTL_HOME` | Node directory. Default `~/.ntl` |
| `NTL_SKIP_DOWNLOAD` | Skip the postinstall download |
| `RUST_LOG` | Log filter, e.g. `RUST_LOG=debug` |

## Notes on install behaviour

**A failed download does not fail `npm install`.** A postinstall script that
exits non-zero breaks installation for the whole project, which is a hostile
thing for a CLI to do. If the download fails you get a warning, and the `ntl`
command explains the problem — with a fix — the first time you run it.

**Checksums are verified, and a mismatch is fatal.** A binary fetched over the
network and then executed is worth being strict about.

## Licence

Apache 2.0. Stewarded by [The Bundu Foundation](https://www.bundu.org).
