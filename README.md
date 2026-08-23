# Neural Transfer Layer (NTL)

**The Neural Transfer Layer for Modern Compute**

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Spec Version](https://img.shields.io/badge/spec-v0.1.0--draft-orange.svg)](https://openntl.org/spec/overview)

---

NTL is an open source data transfer layer that replaces the request-response paradigm of traditional APIs with neural signal propagation. Built for the age of AI, Web3, and quantum computing.

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

NTL is not yet published to crates.io. Build the CLI from a clone:

```bash
git clone https://github.com/openNTL/ntl && cd ntl
cargo install --path runtime/ntl-cli

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
│   ├── ntl-store-sqlite/    # Default storage backend
│   ├── ntl-store-postgres/  # Full-node storage backend (stub)
│   ├── ntl-cli/        # CLI tooling
│   ├── ntl-node/       # Full node binary
│   └── ntl-edge/       # Edge node (lightweight, SQLite by default)
├── adapters/           # Protocol adapters
│   ├── web2/           # HTTP, WebSocket, gRPC, GraphQL
│   ├── web3/           # EVM chains, DID, tokens
│   └── legacy/         # REST/SOAP wrapper
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

NTL is in **Phase 1: Reference Implementation**. Phase 0 is complete: the
specification is at 0.2.0-draft, covering the learning model, threat model,
delivery semantics, and storage interface.

What runs today: two nodes form a synapse over loopback, exchange signed
signals, return receipts, and the routing weights change in response. See the
[roadmap](https://openntl.org/governance/roadmap) for what is not built yet —
notably QUIC transport, post-quantum crypto modules, and a public test
network.

## Contributing

We welcome contributions from anyone, anywhere. See [CONTRIBUTING.md](CONTRIBUTING.md) and our [contribution guide](https://openntl.org/governance/contributing).

NTL is built on the Ubuntu philosophy — *"I am because we are."*

## Built by The Bundu Foundation

NTL is stewarded by [The Bundu Foundation](https://www.bundu.org), an open source foundation building infrastructure for African markets and beyond.

| Entity | Role |
|---|---|
| [The Bundu Foundation](https://www.bundu.org) | Owner and steward |
| [Nyuchi Web Services](https://nws.nyuchi.com) | Engineering, reference implementation |
| [Nyuchi Africa](https://www.nyuchi.com) | Core maintainer |
| [Mukoko Africa](https://mukoko.com) | Core maintainer |
| [SiafuDB](https://siafudb.org) | Ecosystem graph-storage backend |
| [Mukoko](https://mukoko.com) | Application platform |

## License

Apache 2.0 — see [LICENSE](LICENSE).
