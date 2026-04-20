# Updating Zebra crates ~best~ possible practices.

Zaino depends on Zebra as its main validator. Zainod and Zebrad are
tightly coupled. Keeping up-to-date with latest Zebra crates is
a priority for Zaino maintainers. A lesser delta between the
zebra-crates Zaino depends on and their latest ensures that there
are no surprises when new versions of these crates are released.

When there's a spread between latest and supported version of the
Zebra crates we consider that a high priority _tech debt_.

# How to approach updating Zebra crates

Note: We expect readers of this document are familiarized with the [testing](./testing.md)
documentation. If you haven't done so, please familiarize yourselve with that
document first

## Pre-condition: run all test and establish a baseline
Run all tests on `dev` with `cargo nextest run --all-features`

This baseline will tell you which tests are currently passing, failing
and their performance. This will help you identify regressions when
updating these or any other dependencies.

## update `.env.testing-artifacts` to the corresponding version of Zebra
Instructions on how to do this can be found in [testing](./testing.md)
documentation.

## Finding out which crates depend on Zebra crates.
Find out which dependencies use `zebra-*` crates by running
`cargo tree` and spotting the usage of Zebra crates.

## Always specify `all-features` when building

Make sure you build and run the project with `all-features` in
order to catch any posible compile errors early.

## Updating Zingo dependencies.
Zaino makes use of ZingoLabs tooling extensively. This means that
when updating a major dependency on Zaino, "different versions of
crate {NAME} are being used" kind of errors. Use `cargo tree` to
find out the crate {NAME} usage and evaluate a highest common denominator
to update all the affected dependencies to that version.

## Juggling transitive dependencies
### Tonic
Tonic is used in Zebra, Zaino, ZingoLib and Librustzcash. This one is
going to be a challenge. Priotize what works with Zebra and then work
your way down the stack. Tonic can break the `.proto` files downstream if
you notice that there are significant issues consult with Zebra and
[Lightclient Protocol](https://github.com/zcash/lightwallet-protocol) maintainers.

### Prost
Prost is used in conjunction with `tonic` to build gRPC .rs files from `.proto` files
it is also used accross many crates like `zaino-proto`, `zerba-rpc`, zingo `infrastructure` and `zaino-integration-tests`. Zaino can't build without reliably generating the files so it's
important to figure this dependency graph out.

## Updating Librustzcash dependencies.
Always try to stick with the latest tag you can find. Although given Zebra uses Librustzcash
as well as ZingoLib, these may clash. Strategy here is to find the highest common denominator
for the two in a per-crate basis.
