---
default: major
---

# `zainod`: `--config` is one global flag; commands return their exit code

`-c, --config <FILE>` is accepted before or after any subcommand (`zainod -c f start` and `zainod start -c f` are the same). `generate-config` without `-o` writes to the `--config` path, else the default one. `zainodlib::run` no longer installs logging: call `zainodlib::logging::init` first. `zainodlib::verify::run`, `zainodlib::hold::command` and the new `zainodlib::generate_config` return `Result<ExitCode, _>` instead of exiting; `cli::Command` variants carry no config path, and `hold` / `resume` are `Command::Hold(hold::Action)`. `IndexerError` variants drop their `Error` suffix (`Config`, `Metrics`, `Join`), and open-file-limit and header-rule failures have their own variants.
