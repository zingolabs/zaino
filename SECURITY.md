# Security Policy

Zaino is critical infrastructure in the Zcash ecosystem — it sits between the
consensus node and every light wallet. We take security vulnerabilities
seriously and appreciate responsible disclosure.

This policy conforms to the
[Responsible Disclosure](https://github.com/RD-Crypto-Spec/Responsible-Disclosure)
standard (`RD-Crypto-Spec`).

## Reporting a Vulnerability

**Do NOT open a public GitHub issue for security vulnerabilities.**

There are two ways to report:

### GitHub Private Vulnerability Reporting (preferred for most cases)

Submit a report through GitHub's built-in tool:

<https://github.com/zingolabs/zaino/security/advisories/new>

This creates a private advisory visible only to maintainers, and provides a
structured form for severity, description, and affected versions.

### Encrypted Email (for sensitive or cross-project disclosures)

Send a PGP-encrypted email to:

**zingodisclosure@proton.me**

Proton Mail provides end-to-end encryption by default when both sender and
recipient use Proton. For non-Proton senders, you can encrypt your message
using our PGP public key:

```
-----BEGIN PGP PUBLIC KEY BLOCK-----

xjMEZj0VexYJKwYBBAHaRw8BAQdAepugX0ypZXyEhE65sKgCuop80LF8GSOO
0eDG+iwqG/7NNXppbmdvZGlzY2xvc3VyZUBwcm90b24ubWUgPHppbmdvZGlz
Y2xvc3VyZUBwcm90b24ubWU+wowEEBYKAD4FgmY9FXsECwkHCAmQOX0pQubt
5ioDFQgKBBYAAgECGQECmwMCHgEWIQRB7j3aI80BkBVAyUI5fSlC5u3mKgAA
ot0A/24C/7GbAKeo1qbx+e/hHSkNkI/htA0XNpiOf/Nr43e+AQD3gnvBQHgD
huSqdlWYjjr7jh5hvQCeRc/CUKoLE2MXDs44BGY9FXsSCisGAQQBl1UBBQEB
B0Ckih69m55HXPcoLKDfYi1D1GOQvstUqgYG2iAgeFg7YgMBCAfCeAQYFgoA
KgWCZj0VewmQOX0pQubt5ioCmwwWIQRB7j3aI80BkBVAyUI5fSlC5u3mKgAA
vGcA/A3WN88yejxRPbKf+8eISjYWsQML5UQK+7BewC4tbuz6AQCrsGeThqKb
hLMKaxtWrjPfQcAuaBCB3wUtvdvgFk4YAA==
=xKDi
-----END PGP PUBLIC KEY BLOCK-----
```

Fingerprint: `41EE 3DDA 23CD 0190 1540 C942 397D 2942 E6ED E62A`

Use encrypted email when:

- You need anonymity beyond what a GitHub account provides
- The vulnerability affects multiple Zcash ecosystem projects and requires
  coordinated disclosure
- You prefer not to rely on a third-party platform for sensitive details

## Supported Versions

| Version | Supported |
| ------- | --------- |
| latest release | Yes |
| `dev` branch | Best-effort |
| older releases | No |

## Response Timeline

- **Acknowledgment**: within 2 working days of receipt
- **Initial assessment**: within 5 working days
- **Fix timeline**: 30–90 days depending on complexity and coordination needs,
  per the RD-Crypto-Spec standard

We will keep you informed of progress throughout.

## Scope

### In scope

- **zaino-state** — indexing logic, chain state, database integrity
- **zaino-serve** — gRPC and JSON-RPC server, request handling
- **zaino-fetch** — JSON-RPC client, data fetching from backends
- **zaino-proto** — protocol buffer definitions, serialization
- **zainod** — daemon configuration, startup, service orchestration
- Any issue where Zaino serves incorrect, incomplete, or misleading chain data
  to clients
- Privacy leaks through timing, filtering, or correlation of requests
- Denial of service against the indexer

### Out of scope

- Vulnerabilities in upstream dependencies (Zebra, zcashd) — report those to
  the respective projects directly
- Issues requiring physical access to the machine running Zaino
- Social engineering

## Disclosure Policy

We follow coordinated disclosure:

1. Reporter sends vulnerability details privately
2. We acknowledge receipt and begin assessment
3. We develop a fix, optionally in a private GitHub fork
4. We coordinate release timing with affected parties
5. Fix is released, then vulnerability details are published

We will credit reporters in the advisory unless they prefer to remain
anonymous.

## Sending Disclosures

When we discover or receive a vulnerability that affects neighboring Zcash
ecosystem projects, we will make a best-effort attempt to privately notify
those projects following the RD-Crypto-Spec process.

## Bilateral Responsible Disclosure Agreements

We intend to establish bilateral disclosure agreements with the following
projects, consistent with the Zcash ecosystem's coordinated security
practices:

- **Zcash** (Electric Coin Company) — `security@z.cash`
- **Zebra** (Zcash Foundation) — `security@zfnd.org`

These agreements are not yet formalized. Until they are, we will still
make best-effort attempts to coordinate disclosure with affected ecosystem
projects.

## Deviations from the Standard

### Silent Exploitation Protection

Zaino does not participate in consensus and cannot mint or destroy coins.
However, as the data layer between the consensus node and light wallets, a
compromised or buggy indexer could silently:

- Misreport balances by hiding or duplicating notes/UTXOs
- Serve corrupted commitment tree data, causing wallets to produce invalid
  spend proofs and effectively locking users out of shielded funds
- Leak privacy through observable request patterns or selective data omission

These classes of bugs share a key property with monetary-base vulnerabilities:
**exploitation may be difficult to detect**, and affected users may not realize
anything is wrong until significant harm has occurred.

For vulnerabilities in this category, we reserve the right to:

- Restrict advance details shared with bilateral partners to the minimum
  needed for coordination
- Extend the embargo period beyond the standard timeline if broader
  coordination is required
- Delay public disclosure until we have evidence that the vulnerability is not
  being actively exploited

This deviation mirrors the "Monetary Base Protection" clauses in the security
policies of [Zcash](https://github.com/zcash/zcash/blob/master/SECURITY.md)
and [Zebra](https://github.com/ZcashFoundation/zebra/blob/main/SECURITY.md),
adapted for Zaino's role as an indexer rather than a consensus node.
