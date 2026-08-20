# Local workflow integration tests (`act` + podman)

These scripts exercise GitHub Actions workflows end-to-end on your machine,
against self-contained git fixtures — no network, no pushing to GitHub, and
(where possible) no cargo inside the container. They complement `relman`'s unit
tests: the unit tests prove the check logic; these prove the *workflow* wiring
(checkout, base fetch, action composition, the advisory/enforce gate).

## `changeset-check.it.sh`

Integration-tests `.github/workflows/changeset-check.yml` + the
`.github/actions/setup-relman` composite action + the `relman changeset check`
CLI. Three scenarios, asserted on the job's exit code:

| Scenario              | `RELMAN_ENFORCE_CHANGESETS` | Expected job result |
| --------------------- | --------------------------- | ------------------- |
| governed source + covering changeset | `true`  | **succeeds** |
| governed source, no changeset        | `true`  | **fails** (enforcement) |
| governed source, no changeset        | unset   | **succeeds** (advisory warning) |

Run it from anywhere:

```bash
tools/scripts/it/changeset-check.it.sh
```

It builds a static `relman` once, stands up a throwaway fixture repo in a temp
dir (cleaned up on exit), and runs `act` three times. Expect `ALL SCENARIOS
GREEN`.

## Requirements

- **`act`** (tested with v0.2.87) and **rootless podman**.
- The **`catthehacker/ubuntu:act-latest`** image (pulled on first run).
- A `relman.toml`-governed repo checkout (this repo) — the script builds
  `relman` from `tools/relman` for the `x86_64-unknown-linux-musl` target, so a
  Rust toolchain with that target is required. `rustup target add
  x86_64-unknown-linux-musl` is run for you (idempotent).

### podman / act host setup

The script points `act` at the rootless podman socket and works around a couple
of host quirks; you don't need to configure anything, but for reference:

- **podman socket**: `unix:///run/user/$(id -u)/podman/podman.sock`, passed via
  `--container-daemon-socket`. Ensure it is running (`systemctl --user start
  podman.socket`).
- **`DOCKER_CONFIG`**: the script sets `DOCKER_CONFIG` to a throwaway dir
  containing `{}` for every `act` invocation. This sidesteps a
  Docker-Desktop credential helper that otherwise breaks anonymous image pulls.
- **sandbox**: if you run this under a filesystem/network sandbox, disable it —
  rootless podman needs write access to `/run/user/$(id -u)/libpod`.

## How the fixture makes `act` work offline

A few `act`-specific details the harness relies on (worth knowing if you write
another `*.it.sh`):

- **`actions/checkout` under act does not clone.** act copies the *entire
  working tree* — including `.git` **and untracked, non-`.gitignore`d files** —
  into the container at the **same absolute host path** it has on disk. So the
  fixture's `.git` (with its `origin/<base>` tracking ref) and its bare
  `origin.git` both land in the container unchanged.
- **The bare `origin` lives *inside* the working tree** (`<work>/origin.git`),
  untracked and never git-added. Because act preserves absolute paths, the
  in-container `git fetch origin dev` resolves to the very same
  `<work>/origin.git` and succeeds — no remote, no mounts. (A bare repo placed
  *outside* the tree, or one that is `.gitignore`d, is not copied in and the
  fetch fails; mounting it via `--container-options -v` triggers a rootless
  podman pid/`fork` exhaustion, so the in-tree approach is deliberate.)
- **Never `git add -A`** in the fixture: that would stage the untracked
  `origin.git` as an embedded gitlink and corrupt it across branch checkouts.
  Add explicit paths only.
- **The event payload** sets `pull_request.base.ref=dev`, `draft=false`, a
  `number`, and `head.repo.full_name == repository.full_name` (same-repo).
- **Enforcement toggle**: `RELMAN_ENFORCE_CHANGESETS` is a repo *variable*,
  passed to act with `--var RELMAN_ENFORCE_CHANGESETS=true`; omitting it leaves
  `${{ vars.RELMAN_ENFORCE_CHANGESETS }}` empty (advisory).
