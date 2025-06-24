CARGO MAKE

Zaino now includes files for using `cargo make` to assist development.
`cargo make` is a Rust task runner and build tool, enabling scripting to be integrated with cargo - named after the traditional `make` UNIX tool.

It uses a `Makefile.toml` to declare tasks, chain together commands, etc.
See (https://github.com/sagiegurari/cargo-make) for more details.

Requirements:
1. `cargo-make`
`cargo install --force cargo-make` or prebuilt [binary](https://github.com/sagiegurari/cargo-make/releases)
2. `docker` *
https://docs.docker.com/
https://docs.docker.com/engine/install/

You can run `cargo make help` for a list of commands available.

`cargo make compute-image tag`
using `./utils/get-ci-image-tag.sh`, formats what's found in the checked-in `.env.testing-artifacts` file (based on commits of `zcahsd`, `zebrad`, and Rust version) into a docker image tag, prints them to stdout and makes the CARGO_MAKE_IMAGE_TAG environment variable.

`cargo make test`
Runs integration tests in a docker container, defined from `Dockerfile.ci`, and mounts the local Zaino directory on the  host system.

`cargo build-image`
Build the Docker image with current artifact versions

`cargo push-image`
Push the image (used in CI, can be used manually)

`cargo ensure-image-exists`
Check if the required image exists locally, build if not

`cargo copy-bins`
Extract binaries from the image to ./test_binaries/bins

`base-script`
sources `helpers.sh`
runs tests with appropriate image,
will use a docker image that exists locally, if not found, pulls from remoteo

*Note: While docker provides powerful tools for application isolation, it involves a number of important interactions with the host system.
-Containers share the host kernel.
-Docker creates and assigns containers to their own network stack.
-Mounting host filesystems into containers has security and privacy implications.
