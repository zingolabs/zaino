#!/usr/bin/env rust-script
//! Build the zainod OCI image and extract its binary reproducibly.
//!
//! Ported from the former `utils/build_deterministic.sh`. Runs two container
//! builds against `Dockerfile.deterministic` with a pinned platform and
//! `SOURCE_DATE_EPOCH`, forwarding any extra arguments to both builds.
//!
//! Engine selection: honours `CONTAINER_ENGINE=docker|podman` when set,
//! otherwise auto-detects, preferring `podman` and falling back to `docker`.
//! Both engines yield the same two artifacts:
//!   * `build/oci/zainod.tar` — the runtime image as an OCI archive named `zainod`
//!   * `build/zainod`         — the static binary from the `export` stage
#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

const PLATFORM: &str = "linux/amd64";
/// Local tag podman builds the runtime image under before `podman save`.
const IMAGE_REF: &str = "localhost/zainod:deterministic";

#[derive(Clone, Copy)]
enum Engine {
    Docker,
    Podman,
}

fn main() -> Result<(), Box<dyn Error>> {
    let repo_root = repo_root()?;
    let dockerfile = repo_root.join("Dockerfile.deterministic");
    let oci_output = repo_root.join("build/oci");
    let engine = select_engine()?;

    // Extra arguments are forwarded verbatim to both builds.
    let forwarded: Vec<String> = env::args().skip(1).collect();

    std::fs::create_dir_all(&oci_output)?;

    // Build runtime image for `docker run` / `podman run`.
    println!("Building runtime image with {}...", engine.binary());
    let oci_tar = oci_output.join("zainod.tar");
    engine.build_runtime_oci(&dockerfile, &repo_root, &oci_tar, &forwarded)?;

    // Extract binary locally from the export stage.
    println!("Extracting binary with {}...", engine.binary());
    engine.build_export(&dockerfile, &repo_root, &forwarded)?;

    Ok(())
}

impl Engine {
    fn binary(self) -> &'static str {
        match self {
            Engine::Docker => "docker",
            Engine::Podman => "podman",
        }
    }

    /// Build the `runtime` target and write it as an OCI image archive named
    /// `zainod` to `oci_tar`.
    fn build_runtime_oci(
        self,
        dockerfile: &Path,
        repo_root: &Path,
        oci_tar: &Path,
        forwarded: &[String],
    ) -> Result<(), Box<dyn Error>> {
        match self {
            Engine::Docker => {
                let output = format!(
                    "type=oci,rewrite-timestamp=true,force-compression=true,dest={},name=zainod",
                    oci_tar.display()
                );
                run_build(
                    self,
                    dockerfile,
                    repo_root,
                    &["--target", "runtime", "--output", &output],
                    forwarded,
                )
            }
            Engine::Podman => {
                // Podman's `--output` only exports image *contents* (local/tar),
                // not an OCI image archive, so build into local storage under a
                // tag and `podman save` it out. `--source-date-epoch` +
                // `--rewrite-timestamp` reproduce BuildKit's `rewrite-timestamp`.
                run_build(
                    self,
                    dockerfile,
                    repo_root,
                    &[
                        "--target",
                        "runtime",
                        "--source-date-epoch",
                        "1",
                        "--rewrite-timestamp",
                        "--tag",
                        IMAGE_REF,
                    ],
                    forwarded,
                )?;
                let status = Command::new("podman")
                    .args(["save", "--format", "oci-archive", "--output"])
                    .arg(oci_tar)
                    .arg(IMAGE_REF)
                    .status()?;
                if !status.success() {
                    return Err(format!("podman save failed: {status}").into());
                }
                Ok(())
            }
        }
    }

    /// Build the `export` target, writing the binary tree to `build/` under the
    /// repo root (yielding `build/zainod`). Both engines accept the same
    /// `type=local` output spec for a filesystem export.
    fn build_export(
        self,
        dockerfile: &Path,
        repo_root: &Path,
        forwarded: &[String],
    ) -> Result<(), Box<dyn Error>> {
        let local_dest = format!("type=local,dest={}/build", repo_root.display());
        run_build(
            self,
            dockerfile,
            repo_root,
            &["--quiet", "--target", "export", "--output", &local_dest],
            forwarded,
        )
    }
}

/// Run a container `build` against the deterministic Dockerfile with the flags
/// every build shares. Per-build flags and any caller-forwarded args follow.
fn run_build(
    engine: Engine,
    dockerfile: &Path,
    repo_root: &Path,
    per_build: &[&str],
    forwarded: &[String],
) -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::new(engine.binary());
    cmd.arg("build")
        .arg("-f")
        .arg(dockerfile)
        .arg(repo_root)
        .arg("--platform")
        .arg(PLATFORM)
        .args(per_build)
        .args(forwarded)
        // Set per-command rather than mutating the process environment, which
        // would require `unsafe` under the 2024 edition.
        .env("SOURCE_DATE_EPOCH", "1");
    match engine {
        // BuildKit is docker-specific; podman builds with buildah natively.
        Engine::Docker => {
            cmd.env("DOCKER_BUILDKIT", "1");
        }
        // The Dockerfile's `SHELL [... -o pipefail ...]` is honoured only by the
        // docker image format; buildah's default OCI format silently ignores it.
        Engine::Podman => {
            cmd.arg("--format").arg("docker");
        }
    }

    let status = cmd.status()?;
    if !status.success() {
        return Err(format!("{} build failed: {status}", engine.binary()).into());
    }
    Ok(())
}

/// Pick the container engine: `CONTAINER_ENGINE` if set, else the first of
/// `podman`, `docker` found on `PATH`.
fn select_engine() -> Result<Engine, Box<dyn Error>> {
    if let Ok(name) = env::var("CONTAINER_ENGINE") {
        return match name.trim() {
            "docker" => Ok(Engine::Docker),
            "podman" => Ok(Engine::Podman),
            other => Err(format!(
                "unsupported CONTAINER_ENGINE={other:?}; expected `docker` or `podman`"
            )
            .into()),
        };
    }
    if on_path("podman") {
        Ok(Engine::Podman)
    } else if on_path("docker") {
        Ok(Engine::Docker)
    } else {
        Err("no container engine found: install podman or docker, or set CONTAINER_ENGINE".into())
    }
}

/// True when `bin` resolves to a file in one of the `PATH` directories.
fn on_path(bin: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Resolve the repository root via `git rev-parse --show-toplevel`.
fn repo_root() -> Result<PathBuf, Box<dyn Error>> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err("`git rev-parse --show-toplevel` failed (not a git repository?)".into());
    }
    let path = String::from_utf8(output.stdout)?;
    Ok(PathBuf::from(path.trim()))
}
