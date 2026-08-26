# Contributing to NTL

Thank you for your interest in contributing to the Neural Transfer Layer. NTL is built on the Ubuntu philosophy — *"I am because we are."*

## Getting Started

```bash
# Fork and clone
git clone https://github.com/YOUR_USERNAME/ntl.git
cd ntl

# Build
cargo build --workspace

# Run tests
cargo test --workspace

# Run clippy
cargo clippy --workspace --all-features

# Format
cargo fmt --all
```

## Development Requirements

- Rust 1.85+ (install via [rustup](https://rustup.rs)). This is the
  `rust-version` in the workspace manifest and CI has a job pinned to it, so
  raising the floor means changing both — a newly-used API that needs a later
  compiler fails there rather than in a user's build.
- Git

## Making Changes

1. Fork the repository
2. Create a feature branch from `main`
3. Make your changes
4. Add tests for new functionality
5. Ensure all checks pass. These are exactly what CI runs, so a green run
   here is a green run there:
   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-features
   cargo check -p ntl-core --target wasm32-unknown-unknown --no-default-features
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
   cargo check --workspace --all-features    # on 1.85, the declared MSRV
   ```
   The wasm32 check is not optional cosmetics: `ntl-core` is required to build
   for it, which is what keeps an async runtime, a transport, or an ambient
   clock out of the crate. CI fails on a dependency that reintroduces one.

   If you touched `mcp/ntl-postgres-mcp-server`:
   ```bash
   npx tsc --noEmit && npx vitest run && npx wrangler deploy --dry-run --env=""
   ```
   If you touched `npm/ntl-cli`: `npm test`
6. Submit a pull request

## Project Structure

```
ntl/
├── runtime/ntl-core/            # Core library — pure Rust, no runtime assumptions
├── runtime/ntl-net/             # TCP transport, authenticated handshake, sessions
├── runtime/ntl-store-sqlite/    # Default storage backend
├── runtime/ntl-store-postgres/  # Full-node storage backend (stub)
├── runtime/ntl-cli/             # CLI binary
├── runtime/ntl-node/            # Full node binary
├── runtime/ntl-edge/            # Edge node (lightweight)
├── adapters/                    # Adapter crates (web2, web3, legacy) — not implemented
├── mcp/                         # MCP servers
├── npm/                         # npm distribution
├── examples/                    # Example applications
├── benchmarks/                  # Performance benchmarks
├── docs/                        # Mintlify documentation, including docs/spec/
└── rfcs/                        # Protocol change proposals
```

The normative specification is `docs/spec/`, not a top-level `spec/`. There is
no top-level `spec/` directory — the specification pages publish to
[openntl.org/spec](https://openntl.org/spec/overview), so keeping them in
`docs/` avoids a second normative home that would immediately drift from the
published one.

## Specification Changes

Changes to the NTL protocol specification require an RFC. See `rfcs/0000-template.md` for the template and the [RFC process](https://openntl.org/governance/rfc-process) for details.

## Code Style

- Run `cargo fmt` before committing
- Follow clippy recommendations (`cargo clippy`)
- Write doc comments for all public items
- Use `thiserror` for error types
- Prefer `tracing` over `println!` for logging
- No `unsafe` code (enforced by `#![forbid(unsafe_code)]`)

## Testing

- Unit tests go in the same file as the code they test (`#[cfg(test)]` module)
- Integration tests go in `tests/` directories
- Property-based tests use `proptest`
- Benchmarks use `criterion`

## Commit Messages

Use conventional commits:
- `feat:` new feature
- `fix:` bug fix
- `docs:` documentation
- `refactor:` code restructuring
- `test:` adding or updating tests
- `bench:` benchmark changes
- `ci:` CI/CD changes
- `chore:` maintenance

## Code of Conduct

All participants are expected to treat each other with respect, kindness, and good faith. We are building infrastructure for everyone.

## License

By contributing, you agree that your contributions will be licensed under the Apache 2.0 License.
