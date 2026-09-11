# openNTL documentation source

This directory is the [Mintlify](https://mintlify.com) source for
**[openntl.org](https://openntl.org)**. Navigation lives in
[`docs.json`](docs.json); everything else is MDX.

For what openNTL _is_, read the [root README](../README.md).

<!--
This file used to be a copy of the root README. It drifted, as duplicated
files do — by the time anyone noticed it was still advertising spec v0.1.0-draft
and a `cargo install ntl-cli` that does not work, months after both had changed.
Nothing linked to it and it was not in the site navigation, so nothing caught
the divergence. Please keep it a pointer.
-->

## Layout

| Path             | What lives there                                                                   |
| ---------------- | ---------------------------------------------------------------------------------- |
| `spec/`          | Normative protocol specification                                                   |
| `concepts/`      | Explanations of signals, synapses, propagation, topology                           |
| `guides/`        | Task-oriented: quickstart, first signal, storage backends, the Postgres MCP server |
| `research/`      | The reasoning behind the design, including prior art                               |
| `api-reference/` | Rust API surface                                                                   |
| `governance/`    | Roadmap and contributing                                                           |

## Working on the docs

```bash
cd docs
npm run install:mint    # npm install -g mint@4.2.876
npm run dev             # live preview on :11200
```

Two checks gate this directory in CI, in the `Docs site` job — not `Docs`, which
is rustdoc over the Rust workspace and never reads these files:

```bash
npm run validate        # the site builds; strict, fails on warnings too
npm run broken-links    # no dead internal links
```

Run both before pushing; catching a break locally is faster than a round trip.
They are the same commands and the same pinned CLI version that CI runs, so a
pass here means a pass there — keep the pin in `package.json` and in
`.github/workflows/ci.yml` in step when you bump it.

The Mintlify GitHub App posts its own `Mintlify Deployment` check, but that one
reports `skipped` on pull requests: it runs on the deploy branch only. It is not
a pre-merge guarantee, which is what the two commands above are for.

`mint a11y` is deliberately not gated. In `docs.json` the accent keys name the
colour, not the mode — `light` is the accent used _in dark mode_ and `dark` the
one used in light mode — so do not "fix" them by matching key to mode; that is
how they came to be swapped. The two real pairings pass AA (7.82:1 and 6.34:1),
but the command also measures `dark` against the dark background, a pairing that
never renders, so it cannot reach exit 0 whatever the assignment.

Every normative claim in `spec/` should be true of the reference
implementation, or say plainly where it is not — see the _Status of This
Document_ section in [`spec/overview.mdx`](spec/overview.mdx). A specification
that quietly overstates what runs costs more than one that admits a gap.
