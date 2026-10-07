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
- Run `makers lint` before pushing: it runs every lint CI runs (fmt, clippy,
  cargo-deny bans, shellcheck, the Dockerfile openssl ban, code duplication,
  `tools/` lints).
  `git config core.hooksPath .githooks` runs it as a pre-push hook.
- Every PR that changes a released crate carries a changeset; see
  [changeset format](./docs/release/changeset-format.md).
- All CI checks must pass. Review happens in public on the PR.
- A PR from a fork does not dispatch the `zcash/integration-tests` suite,
  because GitHub withholds repository secrets from fork PRs. Its
  `Trigger integration tests` check is skipped, and a skipped check does not
  mean the suite passed. The suite first runs when the change merges to `dev`,
  unless a maintainer pushes the branch to `zingolabs/zaino` first.
- A PR is written by one developer, reviewed in detail by a second, and merged
  by a third. Experienced maintainers may waive this case by case.
- Signed (verified) commits are encouraged; see GitHub's
  [commit signature verification](https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification).
- Keep docs accurate to your latest commit, including doc comments and the
  affected crate's `usage.md`.

## Testing

Zaino uses [`cargo nextest`](https://nexte.st/)
(`cargo install cargo-nextest --locked`):

```sh
cargo nextest run --workspace
```

The live suites run on a Kubernetes cluster through `ztest`; see
[docs/testing.md](./docs/testing.md).

The reference platform is Debian 12 (Bookworm) on `x86_64-unknown-linux-gnu`;
the container image builds on it.

## Software philosophy

Zaino is Free Software. We hold that FOSS, with its freedoms to run, study,
redistribute and improve a program, is the best path to individual and social
freedom in computing, and that building on shared work reduces duplicated
effort and improves quality. See
[FOSS: A General Introduction](https://en.wikibooks.org/wiki/FOSS_A_General_Introduction/Preface).
