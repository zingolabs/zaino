# Testing
### Dependencies
1) [Zebrad](https://github.com/ZcashFoundation/zebra.git)
2) [Lightwalletd](https://github.com/zcash/lightwalletd.git)
3) [Zcashd, Zcash-Cli](https://github.com/zcash/zcash)

### Tests
1) Symlink or copy compiled `zebrad`, `zcashd` and `zcash-cli` binaries to `zaino/test_binaries/bins/*`
2) Add `zaino/test_binaries/bins` to `$PATH` or to `$TEST_BINARIES_DIR`
3) Run `cargo nextest run`

The expected versions of these binaries is detailed in the file ``.env.testing-artifacts`.

## Cargo Make
Another method to work with tests is using `cargo make`, a Rust task runner and build tool.
This can be installed by running `cargo install --force cargo-make` which will install cargo-make in your ~/.cargo/bin.
From that point you will have two executables available: `cargo-make` (invoked with `cargo make`) and `makers` which is invoked directly and not as a cargo plugin.

`cargo make help`
will print a help output.
`Makefile.toml` holds a configuration file.
