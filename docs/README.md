# openNTL documentation source

This directory is the [Mintlify](https://mintlify.com) source for
**[openntl.org](https://openntl.org)**. Navigation lives in
[`docs.json`](docs.json); everything else is MDX.

For what openNTL *is*, read the [root README](../README.md).

<!--
This file used to be a copy of the root README. It drifted, as duplicated
files do — by the time anyone noticed it was still advertising spec v0.1.0-draft
and a `cargo install ntl-cli` that does not work, months after both had changed.
Nothing linked to it and it was not in the site navigation, so nothing caught
the divergence. Please keep it a pointer.
-->

## Layout

| Path | What lives there |
|---|---|
| `spec/` | Normative protocol specification |
| `concepts/` | Explanations of signals, synapses, propagation, topology |
| `guides/` | Task-oriented: quickstart, first signal, storage backends, the Postgres MCP server |
| `research/` | The reasoning behind the design, including prior art |
| `api-reference/` | Rust API surface |
| `governance/` | Roadmap and contributing |

## Working on the docs

```bash
npm i -g mint
cd docs
mint dev
```

Broken internal links fail the `Docs` job in CI, so `mint dev` catching them
locally is faster than a round trip.

Every normative claim in `spec/` should be true of the reference
implementation, or say plainly where it is not — see the *Status of This
Document* section in [`spec/overview.mdx`](spec/overview.mdx). A specification
that quietly overstates what runs costs more than one that admits a gap.
