# Contributing to Zaino

Welcome and thank you for your interest in Zaino! We look forward to your contribution to this important part of the Zcash mainnet and testing ecosystem.

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

## How to Contribute
Code and documentation are very helpful and the lifeblood of Free Software. To merge in code to this repo, one will have to have a [GitHub account](https://docs.github.com/en/account-and-profile), and the ability to cryptographically verify commits against this identity.

The best way to verify is using a GPG signature. See [this document about commit signature verification.](https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification)

Code, being Rust, should be formatted using `rustfmt` and applying the `clippy` suggestions.
For convenience, there are two scripts included in the `utils` directory which run these tools and remove trailing whitespaces.
From the project's workspace root, you can run `./utils/precommit_check.sh`

Code should be as complex as it needs to be, but no more.

All code will be reviewed in public, as conversations on the pull request. It is very possible there will be requested changes or questions. This is not a sign of disrespect, but is necessary to keep code quality high in an important piece of software in the Zcash ecosystem.

Documentation should be clear and accurate to the latest commit on `dev`.

These contributions must be [GitHub pull requests](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/proposing-changes-to-your-work-with-pull-requests/about-pull-requests) opened _from a personal fork_ of the project, _to this repo, zingolabs/zaino_. Generally pull requests will be against `dev`, the development branch.

## Bug Reports and Feature Requests

If you believe you have discovered a security issue and wish to disclose it non-pubicly, please contact us at:
zingodisclosure@proton.me

Bug reports and feature requests can best be opened as [issues](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/creating-an-issue) on this GitHub repo. To do so you will need a [GitHub account](https://docs.github.com/en/account-and-profile). Especially for bug reports, any details you can offer will help us understand the issue better. Such details include versions or commits used in exposing the bug, what operating system is being used, and so on.

Bug reports and feature requests can also be registered via other [communication channels](#communication-channels), but will be accepted in this way without guarantees of visibility to project software developers.

## Local Testing
Local testing requires a system with ample resources, particularly RAM.

Zaino uses [`cargo nextest`](https://nexte.st/). On the linux command line, with a system already using Rust (and `cargo`), you can install this using `cargo install cargo-nextest --locked` or from GitHub with `cargo install --git https://github.com/nextest-rs/nextest --bin cargo-nextest`.

After installing this crate, all tests can be run locally with `cargo nextest run`.

For more details see our [testing document](./docs/testing.md).

## Communication Channels
In addition to GitHub, there is a ZingoLabs [Matrix](https://matrix.org/) channel that can be reached through [this web link](https://matrix.to/#/!cVsptZxBgWgmxWlHYB:matrix.org). Our primary languages are English and Spanish.

Other channels where you may be able to reach Zingolabs developers that include the [Zcash Community Forum](https://forum.zcashcommunity.com/) website, Bluesky, Telegram and X (English and Spanish), Instagram (Spanish), and Zcash related Discord.

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
