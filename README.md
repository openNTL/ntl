# Neural Transfer Layer (NTL)

> The Neural Transfer Layer for Modern Compute

[![CI](https://github.com/openNTL/ntl/actions/workflows/ci.yml/badge.svg)](https://github.com/openNTL/ntl/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://www.apache.org/licenses/LICENSE-2.0)
[![Spec Version](https://img.shields.io/badge/spec-beta__0.0.0-blue.svg)](https://openntl.org/spec/overview)
[![npm](https://img.shields.io/badge/npm-%40bundu%2Fntl--cli-red.svg)](https://www.npmjs.com/package/@bundu/ntl-cli)
![Rust](<https://img.shields.io/badge/Rust-1.85+_(2024_edition)-000000?logo=rust&logoColor=white>)

**Version:** 0.2.0-beta.1 (spec `beta_0.0.0`) | **Docs:** [openntl.org](https://openntl.org) | **CLI:** [`@bundu/ntl-cli`](https://www.npmjs.com/package/@bundu/ntl-cli) | **Licence:** Apache 2.0

---

NTL is an open source data transfer layer that replaces the request-response paradigm of traditional APIs with neural signal propagation. Built for the age of AI, Web3, and quantum computing.

The Rust reference implementation is a Cargo workspace of twelve members,
all at `0.2.0-beta.1`, all Apache 2.0, all with `unsafe_code = "forbid"` at
the workspace root. `rust-version` is 1.85, edition 2024.

## Why NTL?

Every major data transfer protocol in use today — HTTP, REST, GraphQL, gRPC — was designed for a world of clients and servers, requests and responses. That world is ending.

NTL introduces:

- **Signals** instead of requests — typed, weighted, cryptographically signed payloads
- **Synapses** instead of connections — persistent channels that strengthen with use
- **Activation thresholds** instead of rate limiting — intelligent, adaptive flow control
- **Emergent routing** instead of endpoint registries — the network self-organizes
- **Pluggable cryptography** — post-quantum ready, no hardcoded schemes

## Architecture

```
┌───────────────────────────────────────────────────┐
│                  Applications                     │
│               (Mukoko, dApps, AI)                 │
├───────────────────────────────────────────────────┤
│             Neural Transfer Layer                 │  ← This project
│              (Signal Transport)                   │
├───────────────────────────────────────────────────┤
│                 Storage Layer                     │
│   pluggable: SQLite │ Postgres │ graph DB │ KV    │
├───────────────────────────────────────────────────┤
│               Network / Hardware                  │
│              (TCP/UDP/QUIC substrate)             │
└───────────────────────────────────────────────────┘
```

**NTL moves signals; your storage layer remembers them.** Storage is a
trait ([`NodeStore`](runtime/ntl-core/src/store/mod.rs)), not a dependency.
The default backend is SQLite — a single file, zero configuration, and it
runs on constrained edge devices. Full nodes can swap in PostgreSQL; graph
and KV backends implement the same trait.

NTL runs on the databases you already trust. See
[storage backends](https://openntl.org/guides/storage-backends).

## Quick Start

```bash
npm install -g @bundu/ntl-cli
```

Or build from source, which always works if you have Rust:

```bash
cargo install --git https://github.com/openNTL/ntl ntl-cli
```

Then:

```bash

# Initialize a node — creates identity + SQLite store at ~/.ntl
ntl init

# Start (development mode)
ntl start --dev

# Emit a signal
ntl emit --type data --payload '{"hello": "world"}'

# Listen for signals
ntl listen
```

Want to see the network learn? Run the two-node demo, which prints synapse
weights strengthening as signals repeat:

```bash
cargo run --example two-node-learning
```

## Documentation

Full documentation is available at [openntl.org](https://openntl.org).

- [Introduction](https://openntl.org/introduction)
- [Why NTL](https://openntl.org/why-ntl)
- [Architecture](https://openntl.org/architecture)
- [Core Concepts](https://openntl.org/concepts/signals)
- [Specification](https://openntl.org/spec/overview)
- [Quickstart Guide](https://openntl.org/guides/quickstart)

## Repository Structure

```
ntl/
├── runtime/            # Rust reference implementation
│   ├── ntl-core/       # Core library — pure Rust, no runtime assumptions
│   ├── ntl-net/        # TCP transport, authenticated handshake, sessions
│   ├── ntl-store-sqlite/    # Default storage backend
│   ├── ntl-store-postgres/  # Full-node storage backend (stub)
│   ├── ntl-cli/        # CLI tooling
│   ├── ntl-node/       # Full node binary
│   └── ntl-edge/       # Edge node (lightweight, SQLite by default)
├── adapters/           # Protocol adapters
│   ├── web2/           # HTTP, WebSocket, gRPC, GraphQL
│   ├── web3/           # EVM chains, DID, tokens
│   └── legacy/         # REST/SOAP wrapper
├── mcp/                # MCP servers
│   └── ntl-postgres-mcp-server/  # Postgres MCP on Workers — also a template
├── npm/                # npm distribution
│   └── ntl-cli/        # @bundu/ntl-cli
├── docs/               # Mintlify documentation source
│   ├── spec/           # Protocol specification (normative)
│   └── research/       # Design research and prior art
├── rfcs/               # Request for Comments
├── examples/           # Example applications
└── benchmarks/         # Performance benchmarks
```

The normative protocol specification lives in [`docs/spec/`](docs/spec/) and
is published at [openntl.org/spec/overview](https://openntl.org/spec/overview).

## Project Status

NTL is in **beta**. The specification and the implementation are versioned
independently — spec `beta_0.0.0`, crates and packages `0.2.0-beta.1` — because
a wording clarification is not a release and a bug fix is not a protocol
revision. Phase 0 is complete: the specification covers the learning model,
threat model, delivery semantics, and storage interface.

What runs today: two nodes form a synapse over loopback, exchange signed
signals, return receipts, and the routing weights change in response. See the
[roadmap](https://openntl.org/governance/roadmap) for what is not built yet —
notably QUIC transport, post-quantum crypto modules, and a public test
network.

## Contributing

We welcome contributions from anyone, anywhere. See [CONTRIBUTING.md](CONTRIBUTING.md) and our [contribution guide](https://openntl.org/governance/contributing).

NTL is built on the Ubuntu philosophy — _"I am because we are."_

## Built by The Bundu Foundation

NTL is a core technical project of [The Bundu Foundation](https://www.bundu.org),
an open source foundation building infrastructure for African markets and
beyond. It is listed among the Foundation's projects at
[bundu.org/projects](https://www.bundu.org/projects/):

> **Neural Transfer Layer (NTL)** — Signal-based data transfer for
> decentralised networks. Replaces APIs with neural propagation.

| Entity                                        | Role                                                   |
| --------------------------------------------- | ------------------------------------------------------ |
| [The Bundu Foundation](https://www.bundu.org) | Owner and steward                                      |
| [Nyuchi](https://www.nyuchi.com)              | Operator; engineering and the reference implementation |
| [Mukoko](https://mukoko.com)                  | Core maintainer                                        |

### Sibling Foundation projects

NTL is storage-agnostic and depends on none of these. They are listed because
several are natural companions, and because the Foundation's projects are
designed to compose.

| Project                                                                                  | What it is                                                                            | Relationship to NTL        |
| ---------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- | -------------------------- |
| [SiafuDB](https://siafudb.org) ([`siafuDB/siafudb`](https://github.com/siafuDB/siafudb)) | Embedded property graph database for device, edge, and Web3; pure Rust, offline-first | One storage backend option |
| [SiafuDB-Kuzu](https://github.com/siafuDB/siafudb-kuzu)                                  | The C++ engine fork: Cypher, vector search, WASM                                      | One storage backend option |
| [Mzizi](https://mzizi.dev)                                                               | Open architecture and design system — a Bundu Foundation project, operated by Nyuchi  | Candidate application      |
| [Mukoko](https://mukoko.com)                                                             | Application platform                                                                  | Application built on NTL   |

Nyuchi Honeycomb (decentralised storage for Web3 pods) and Harare Metro
(public-transport routing) are named in Foundation ecosystem copy but have
no public repository yet, so they are not linked here.

## License

Apache 2.0 — see [LICENSE](LICENSE).
