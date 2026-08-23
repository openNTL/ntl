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

## Quick Start

```bash
# Install
cargo install ntl-cli

# Initialize a node
ntl init

# Start (development mode)
ntl start --dev

# Emit a signal
ntl emit --type data --payload '{"hello": "world"}'

# Listen for signals
ntl listen
```

## Documentation

Full documentation is available at [openntl.org](https://openntl.org).

- [Introduction](https://openntl.org/introduction)
- [Why NTL](https://openntl.org/why-ntl)
- [Architecture](https://openntl.org/architecture)
- [Core Concepts](https://openntl.org/concepts/signals)
- [Specification](https://openntl.org/spec/overview)
- [Quickstart Guide](https://openntl.org/guides/quickstart)

### Run the docs locally

The Mintlify dev server for NTL is pinned to **port 11200** so it can run alongside sibling project docs (port 11300).

```bash
cd docs
npm run install:mintlify   # once — global install
npm run dev                # http://localhost:11200
```

## Repository Structure

```
ntl/
├── spec/               # Protocol specification documents
├── runtime/            # Rust reference implementation
│   ├── ntl-core/       # Core library
│   ├── ntl-cli/        # CLI tooling
│   ├── ntl-node/       # Full node binary
│   └── ntl-edge/       # Edge node (lightweight)
├── adapters/           # Protocol adapters
│   ├── web2/           # HTTP, WebSocket, gRPC, GraphQL
│   ├── web3/           # EVM chains, DID, tokens
│   └── legacy/         # REST/SOAP wrapper
├── docs/               # Mintlify documentation source
├── rfcs/               # Request for Comments
├── examples/           # Example applications
└── benchmarks/         # Performance benchmarks
```

## Project Status

NTL is in **Phase 0: Foundation** — specification development and documentation. See the [roadmap](https://openntl.org/governance/roadmap) for details.

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
