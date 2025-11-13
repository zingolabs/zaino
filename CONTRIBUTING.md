# Contributing to Zaino

Welcome! Thank you for your interest in Zaino. We look forward to your contribution to this important part of the Zcash mainnet and testing ecosystem.

## Table of Contents
- [Getting Started](#getting-started)
- [How to Contribute Code and Documentation](#how-to-contribute)
- [How to open Bug Reports and Feature Requests](#bug-reports-and-feature-requests)
- [Local Testing](#local-testing)
- [Communication Channels](#communication-channels)
- [More Documentation](#more-documentation)
- [Software Philosophy](#software-philosophy)

## Getting Started
To get started using Zaino, please see our [use cases document](./docs/use_cases.md) where you can find instructions for use and example use cases.

We welcome and appreciate contributions in the form of code, documentation, bug reports and feature requests. We also generally enjoy feedback and outreach efforts.

## Bug Reports and Feature Requests

If you believe you have discovered a security issue and wish to disclose it non-pubicly, please contact us at:
zingodisclosure@proton.me

Bug reports and feature requests can best be opened as [issues](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/creating-an-issue) on this GitHub repo. To do so you will need a [GitHub account](https://docs.github.com/en/account-and-profile). Especially for bug reports, any details you can offer will help us understand the issue better. Such details include versions or commits used in exposing the bug, what operating system is being used, and so on.

Bug reports and feature requests can also be registered via other [communication channels](#communication-channels), but will be accepted in this way without guarantees of visibility to project software developers.

## Communication Channels
In addition to GitHub, there is a ZingoLabs [Matrix](https://matrix.org/) channel that can be reached through [this web link](https://matrix.to/#/!cVsptZxBgWgmxWlHYB:matrix.org). Our primary languages are English and Spanish.

Other channels where you may be able to reach Zingolabs developers that include the [Zcash Community Forum](https://forum.zcashcommunity.com/) website, Bluesky, Telegram and Twitter/X (English and Spanish), Instagram (Spanish), and Zcash related Discord.

## How to Contribute
Code and documentation are very helpful and the lifeblood of Free Software. To merge in code to this repo, one will have to have a [GitHub account](https://docs.github.com/en/account-and-profile).

Code, being Rust, must be formatted using `rustfmt` and applying the `clippy` suggestions.
For convenience, there are scripts included in the `utils` directory which run these tools and remove trailing whitespaces. From the project's workspace root, you can run `./utils/precommit-check.sh`

In general, PRs should be opened against [the `dev` branch](https://github.com/zingolabs/zaino/tree/dev).

All tests must pass, see [Local Testing](#local-testing).

Verified commits are encouraged. The best way to verify is using a GPG signature. See [this document about commit signature verification.](https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification)

Code should be as complex as it needs to be, but no more.

All code will be reviewed in public, as conversations on the pull request. It is very possible there will be requested changes or questions. This is not a sign of disrespect, but is necessary to keep code quality high in an important piece of software in the Zcash ecosystem.

Documentation should be clear and accurate to your latest commit. This includes sensible and understandable doc comments.

Contributions must be [GitHub pull requests](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/proposing-changes-to-your-work-with-pull-requests/about-pull-requests). New contributors should make PRs _from a personal fork_ of the project, _to this repo, zingolabs/zaino_. Generally pull requests will be against `dev`, the development branch.

When code or documentation is still being developed and is not intended for review, the PR should be in the `Draft` state.
`Draft` pull requests cannot be merged: an `Open` PR is a PR that is "ready for review." The `Draft` state should be set as a default when working with GitHub on the web, and can be changed to `Open` status later, marking the PR as ready for review.

All CI checks (remote testing and lints) must pass.

Running `cargo update` may be considered as a requirement for some releases.

PRs should be written by one developer, have a detailed review performed by a second developer, and be checked over and merged by a third.

Certain situations may arise where experienced Zaino developers might bypass the merge-constraints, on a case-by-case basis.

Finally, see our [Software Philosophy](#software-philosophy), and understand you are contribuing to a project with these principles at work.

## Block Explorer Merge Requirements
This is an evolving document: the following merge requirements are intended for the specific case of Block Explorer RPCs.

Any use of `TODO`s makes a PR invalid for merging into `dev`.

Use of `.unwrap()` and `.expect()` are discouraged in non-test code. When used in code, an explicit comment is required explaining why the particular use of expect is okay, (eg matching on a known enum variant).
In test code, `.unwrap()` is wrong when a helper function might fail with insufficient information.

Doc-tested doc-comments should be used to avoid stale docs, and skew from the underlying code. Quality doc-comments should include a doc-test, and with `pub` interface doc-comments should be considered nearly as a requirement.

Error handling must be included and expose underlying information as much as and wherever possible, to assist developers and users.

Merges must minimally reflect the zcash RPC spec and include a link to the relevant zcash C++ implementation (URLs that point at the analogous logic), OR reflect the C++ implementation.

Tests are encouraged that show parity bewteen responses from `zcash-cli` + `zcashd` and `zaino`+ a `zebra` backend, and the local cache.

## Local Testing
Local testing requires a system with ample resources, particularly RAM.

Tier 1 denotes the reference platform. It is the latest, updated, stable [Debian 12](https://www.debian.org/releases/bookworm/), codename Bookworm, with an AMD64 `x86_64-unknown-linux-gnu` compilation target. This can be thought of as Tier 1 or "guaranteed to build and pass all tests."

Tier 2 platforms are platforms that are currently understood to be working as well as Tier 1, but as non-canonical sources of truth. Sometimes these platforms provide valuable insights when compared with the reference Tier 1 Debian. Therefore, using them is encouraged.

Currently, [Arch Linux](https://archlinux.org) AMD64 `x86_64-unknown-linux-gnu` is understood to be Tier 2.

Zaino uses [`cargo nextest`](https://nexte.st/). On the linux command line, with a system already using Rust (and `cargo`), you can install this using `cargo install cargo-nextest --locked` or from GitHub with `cargo install --git https://github.com/nextest-rs/nextest --bin cargo-nextest`.

After installing this crate, all tests can be run locally with `cargo nextest run`.

For more details see our [testing document](./docs/testing.md).

## More Documentation

To see more included documentation, please see [our docs directory](./docs/).
## Software Philosophy
We believe in the power of Free and Open Source Software (FOSS) as the best path for individual and social freedom in computing.

Very broadly, Free Software provides a clear path to make software benefit its users. That is, Free Software  has the possibility to be used it like a traditional tool, extending the user's capabilities, unlike closed source software which constrains usage, visability and adaptability of the user while providing some function.

In more detail, the Free Software Foundation states FOSS allows:

The freedom to run a program, for any purpose,

The freedom to study how a program works and adapt it to a person’s needs. Access to the source code is a precondition for this,

The freedom to redistribute copies so that you can help your neighbour,  and

The freedom to improve a program and release your improvements to the public, so that the whole community benefits. Access to the source code is a precondition for this.

Developing from this philosophical perspective has several practical advantages:

Reduced duplication of effort,

Building upon the work of others,

Better quality control,

Reduced maintenance costs.

To read more, see [this document on wikibooks](https://en.wikibooks.org/wiki/FOSS_A_General_Introduction/Preface).
