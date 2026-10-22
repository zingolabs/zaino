# Contributing to Zaino

Contributions of code, documentation, bug reports and feature requests are
welcome. To run Zaino, start with [running zainod](./docs/running.md).

## Security issues

Do not open a public issue. Report privately via
[GitHub security advisories](https://github.com/zingolabs/zaino/security) or
email zingodisclosure@proton.me.

## Bug reports and feature requests

Open a GitHub [issue](https://github.com/zingolabs/zaino/issues) using one of
the templates. For bugs, include the Zaino version or commit, the zebrad
version, your OS, and your config.

## Communication

The ZingoLabs [Matrix channel](https://matrix.to/#/!cVsptZxBgWgmxWlHYB:matrix.org)
(English and Spanish), and the
[Zcash Community Forum](https://forum.zcashcommunity.com/).

## Pull requests

- Open PRs against [`dev`](https://github.com/zingolabs/zaino/tree/dev), from a
  personal fork if you are a new contributor. Keep a PR in `Draft` until it is
  ready for review.
- Run the lints before pushing ([Development](#development)).
- Every PR that changes a released crate carries a change file (`knope document-change`); see
  [RELEASING.md](./RELEASING.md#every-pr-a-change-file).
- All CI checks must pass. Review happens in public on the PR.
- A PR is written by one developer, reviewed in detail by a second, and merged
  by a third. Experienced maintainers may waive this case by case.
- Signed (verified) commits are encouraged; see GitHub's
  [commit signature verification](https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification).
- Keep docs accurate to your latest commit, including doc comments and the
  affected crate's `usage.md` and, for system-level behaviour,
  `docs/architecture.md`.

## Development

`nix develop` gives the pinned Rust toolchain and every tool CI uses, at the versions in
`flake.lock`. CI runs each job inside that same shell. Without Nix, rustup picks up
`rust-toolchain.toml`, but the other tools (`pre-commit`, `cargo-nextest`, `cargo-deny`,
`cargo-hack`, `code-dupes`, `protoc`) are yours to install, and a version other than the pinned
one may lint differently from CI.

Every lint is a hook in [`.pre-commit-config.yaml`](./.pre-commit-config.yaml), and CI's `Lint`
job runs exactly these:

```sh
pre-commit install                                   # once: fast hooks on commit, clippy on push
pre-commit run --all-files --hook-stage pre-push     # every lint, as CI runs it
cargo nextest run --workspace --all-features         # CI's `Test` job
```

A duplicate the `code-dupes` hook reports is fixed by sharing the code. When sharing would make
it worse, `code-dupes ignore <fingerprint> --reason "..."` records a reviewed exception in
`.dupes-ignore.toml`.

The live suites run on a Kubernetes cluster through `ztest`, never in GitHub CI; see
[docs/testing.md](./docs/testing.md).

The reference platform is Debian 12 (Bookworm) on `x86_64-unknown-linux-gnu`;
the container image builds on it.
