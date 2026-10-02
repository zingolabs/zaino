//! Bakes the resolved zebrad into a `zebra!()` macro for the live suite.
//!
//! The resolution itself is in `zebra_spec.rs`, which carries its own unit
//! tests; this script is only the file, environment and codegen boundary.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::{env, fs};

include!("zebra_spec.rs");

/// The live-tests workspace manifest, holding `[workspace.metadata.zaino.zebra]`.
const WORKSPACE_MANIFEST: &str = "../Cargo.toml";

/// The root workspace lockfile. Read for `source = "patch"`: cargo resolves a
/// `[patch.crates-io]` branch to a commit and records it here, which is the
/// commit zainod itself was built from.
const ROOT_LOCK: &str = "../../Cargo.lock";

/// Generated beside the crate's other build output, `include!`d by `src/zebra.rs`.
const GENERATED: &str = "zebra_macro.rs";

fn main() {
    println!("cargo::rerun-if-changed=zebra_spec.rs");
    println!("cargo::rerun-if-changed={WORKSPACE_MANIFEST}");
    println!("cargo::rerun-if-changed={ROOT_LOCK}");
    for key in [ENV_SOURCE, ENV_VERSION, ENV_GIT, ENV_REV, ENV_DOCKERFILE] {
        println!("cargo::rerun-if-env-changed={key}");
    }

    if let Err(err) = generate() {
        // One line per link in the chain, so a typed cause is not flattened away.
        println!("cargo::error=zebra validator: {err}");
        let mut cause = err.source();
        while let Some(link) = cause {
            println!("cargo::error=  caused by: {link}");
            cause = link.source();
        }
        std::process::exit(1);
    }
}

fn generate() -> Result<(), BuildError> {
    let manifest = read(Path::new(WORKSPACE_MANIFEST))?.ok_or(BuildError::MissingManifest {
        path: WORKSPACE_MANIFEST,
    })?;
    let lock = read(Path::new(ROOT_LOCK))?;

    let env: BTreeMap<String, String> = [ENV_SOURCE, ENV_VERSION, ENV_GIT, ENV_REV, ENV_DOCKERFILE]
        .into_iter()
        .filter_map(|key| env::var(key).ok().map(|value| (key.to_string(), value)))
        .collect();

    let spec = resolve(&manifest, lock.as_deref(), &env)?;
    if let Origin::Git { url, rev } = &spec.origin {
        println!("cargo::warning=zebrad is built from {url} at {rev}, not a published release");
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or(BuildError::NoOutDir)?).join(GENERATED);
    fs::write(&out, render(&spec)).map_err(|source| BuildError::Write { path: out, source })
}

/// `Ok(None)` when the file is absent; an unreadable file stays an error.
fn read(path: &Path) -> Result<Option<String>, BuildError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(BuildError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Debug, thiserror::Error)]
enum BuildError {
    #[error("{path} is missing; it declares [workspace.metadata.zaino.zebra]")]
    MissingManifest { path: &'static str },

    #[error("could not read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cargo set no OUT_DIR")]
    NoOutDir,

    #[error("could not write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Spec(#[from] SpecError),
}
