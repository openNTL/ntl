# Security Policy

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.**

Report vulnerabilities privately to **security@openntl.org**. Include:

- A description of the issue and its impact
- Steps to reproduce, or a proof of concept
- Affected versions, commits, or specification sections
- Any suggested mitigation

You will receive an acknowledgement within **5 business days**. We aim to
provide an assessment and remediation plan within **30 days** of
acknowledgement. We will credit reporters in the release notes unless you
ask us not to.

## Scope

NTL is a protocol project with a reference implementation. Both are in
scope:

| Area | Examples |
|---|---|
| Specification | Attacks the protocol permits by design; ambiguities that lead implementers into insecure behaviour |
| Reference implementation | Memory safety, signature verification bypass, panics reachable from untrusted input, storage-layer injection |
| Learned routing | Weight poisoning, Sybil influence, eclipse attacks on topology knowledge |

The **threat model** is normative and lives at
[spec/threat-model](https://openntl.org/spec/threat-model). Read it before
reporting: it states what NTL defends against at this version and — just
as importantly — **what it explicitly does not defend against yet**. A
report describing an out-of-scope attack is still welcome, but will be
triaged as a roadmap item rather than a vulnerability.

## Supported Versions

NTL has not yet reached a stable release. Until v1.0, only the `main`
branch receives security fixes.

| Version | Supported |
|---|---|
| `main` | Yes |
| Pre-v1.0 tags | No — upgrade to `main` |

## Cryptography

NTL ships pluggable cryptography with post-quantum defaults. See
[spec/crypto-interface](https://openntl.org/spec/crypto-interface) and
[security/post-quantum](https://openntl.org/security/post-quantum).

Weaknesses in a *pluggable module* that NTL merely offers as an option are
in scope for documentation fixes and default changes. Weaknesses in the
underlying primitives (e.g. a break in ML-DSA) should be reported upstream;
tell us too, so we can change defaults.

## Safe Harbour

We will not pursue legal action against researchers who:

- Act in good faith and avoid privacy violations, data destruction, and
  service degradation
- Test only against their own nodes, or nodes whose operators have
  consented
- Give us reasonable time to remediate before public disclosure

Do not test against the public bootstrap nodes without contacting us
first.
