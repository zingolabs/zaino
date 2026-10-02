// Resolves which zebrad the live suite stands up.
//
// Pure: every input arrives as text, so the whole resolution is unit-testable.
// `build.rs` performs the file and environment reads, `include!`s this, and
// bakes the result into a `zebra!()` macro. A macro rather than a function
// because `ztest::dev!` parses its `git` / `rev` / `version` arguments as
// string literals, so the call site cannot read an environment variable.
//
// `//` and not `//!`: `include!` splices this mid-file, where an inner doc
// comment is a parse error.

use std::collections::BTreeMap;

const PUBLISHED: &str = "published";
const PATCH: &str = "patch";
const DEFAULT_DOCKERFILE: &str = "docker/Dockerfile";
const DEFAULT_CONTEXT: &str = ".";

const ENV_SOURCE: &str = "ZAINO_ZEBRA_SOURCE";
const ENV_VERSION: &str = "ZAINO_ZEBRA_VERSION";
const ENV_GIT: &str = "ZAINO_ZEBRA_GIT";
const ENV_REV: &str = "ZAINO_ZEBRA_REV";
const ENV_DOCKERFILE: &str = "ZAINO_ZEBRA_DOCKERFILE";

/// Where the zebrad image comes from.
#[derive(Debug, PartialEq, Eq)]
enum Origin {
    /// The `zfnd/zebra:<version>` release tag.
    Published,
    /// Built on-cluster from a pushed commit. `rev` is always a resolved SHA:
    /// ztest keys its fetch cache and its image tag on this string and treats
    /// both as immutable, which holds for a commit and not for a branch.
    Git { url: String, rev: String },
}

/// The resolved zebrad the generated `zebra!()` macro stands up.
#[derive(Debug, PartialEq, Eq)]
struct ZebraSpec {
    /// Release semver. Also gates which NU6.x activation-height keys ztest
    /// emits into the regtest config, so a fork build declares one too.
    version: String,
    dockerfile: String,
    context: String,
    origin: Origin,
}

/// What `[workspace.metadata.zaino.zebra]` declares.
#[derive(Debug, PartialEq, Eq)]
struct Metadata {
    source: String,
    version: String,
    dockerfile: String,
    context: String,
}

/// Why resolution could not produce a [`ZebraSpec`].
#[derive(Debug, thiserror::Error)]
enum SpecError {
    #[error("live-tests/Cargo.toml is not valid TOML")]
    Manifest {
        #[source]
        source: toml::de::Error,
    },

    #[error("the root Cargo.lock is not valid TOML")]
    Lock {
        #[source]
        source: toml::de::Error,
    },

    #[error(
        "live-tests/Cargo.toml has no [workspace.metadata.zaino.zebra]; \
         the suite has no zebrad to stand up"
    )]
    NoMetadata,

    #[error("[workspace.metadata.zaino.zebra] {key} must be a string")]
    MetadataType { key: &'static str },

    /// Not defaulted: a default here would be the hidden pin this table exists
    /// to replace.
    #[error(
        "[workspace.metadata.zaino.zebra] has no `version`; it is the image tag \
         and the semver gating NU6.x regtest config, so it must be declared"
    )]
    NoVersion,

    #[error("zebra source = \"{0}\"; expected \"published\" or \"patch\"")]
    UnknownSource(String),

    #[error("{present} is set without {missing}; a fork build needs both a url and a commit")]
    PartialGitOverride {
        present: &'static str,
        missing: &'static str,
    },

    #[error("source = \"patch\" but the root Cargo.lock was not readable")]
    NoLock,

    #[error(
        "source = \"patch\" but no zebra crate in the root Cargo.lock has a git source; \
         add [patch.crates-io] zebra-chain = {{ git = \"<fork>\", branch = \"<branch>\" }}, \
         or set source = \"published\""
    )]
    NoGitPatch,

    /// Refused rather than silently running a released zebrad against a
    /// zainod built from forked libraries.
    #[error(
        "source = \"patch\" but zebra crate `{0}` is patched to a local path; the cluster \
         cannot reach your worktree, so push the fork and patch it by git url + branch"
    )]
    PathPatch(String),

    #[error(
        "zebra crates are patched to differing commits ({a}, {b}); the validator and the \
         indexer must come from one tree"
    )]
    DivergentPatch { a: String, b: String },

    #[error("git source `{0}` has no `#<sha>` fragment")]
    UnresolvedGitSource(String),
}

fn is_zebra(name: &str) -> bool {
    name == "zebrad" || name.starts_with("zebra-")
}

fn check_source(source: &str) -> Result<(), SpecError> {
    match source {
        PUBLISHED | PATCH => Ok(()),
        other => Err(SpecError::UnknownSource(other.to_string())),
    }
}

/// Splits a lockfile `git+` source into its url and resolved commit.
///
/// Cargo records the commit in the `#` fragment whatever the manifest asked
/// for, so a `branch = ".."` patch arrives here already resolved.
fn parse_git_source(source: &str) -> Result<(String, String), SpecError> {
    let body = source.strip_prefix("git+").unwrap_or(source);
    let (locator, rev) = body
        .rsplit_once('#')
        .ok_or_else(|| SpecError::UnresolvedGitSource(source.to_string()))?;
    let url = locator.split_once('?').map_or(locator, |(url, _)| url);
    Ok((url.to_string(), rev.to_string()))
}

/// The url and commit every git-sourced `zebra-*` package in the lockfile
/// agrees on.
fn patch_from_lock(lock: &str) -> Result<(String, String), SpecError> {
    let doc: toml::Table = lock.parse().map_err(|source| SpecError::Lock { source })?;
    let packages = doc.get("package").and_then(toml::Value::as_array);

    let mut found: Option<(String, String)> = None;
    for package in packages.into_iter().flatten() {
        let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        if !is_zebra(name) {
            continue;
        }
        // A registry dependency carries `source`; a path patch carries none.
        let Some(source) = package.get("source").and_then(toml::Value::as_str) else {
            return Err(SpecError::PathPatch(name.to_string()));
        };
        if !source.starts_with("git+") {
            continue;
        }
        let git = parse_git_source(source)?;
        match &found {
            Some((_, seen)) if *seen != git.1 => {
                return Err(SpecError::DivergentPatch {
                    a: seen.clone(),
                    b: git.1,
                });
            }
            Some(_) => {}
            None => found = Some(git),
        }
    }
    found.ok_or(SpecError::NoGitPatch)
}

/// Reads `[workspace.metadata.zaino.zebra]`.
fn metadata(manifest: &str) -> Result<Metadata, SpecError> {
    let doc: toml::Table = manifest
        .parse()
        .map_err(|source| SpecError::Manifest { source })?;
    let table = doc
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("metadata"))
        .and_then(toml::Value::as_table)
        .and_then(|metadata| metadata.get("zaino"))
        .and_then(toml::Value::as_table)
        .and_then(|zaino| zaino.get("zebra"))
        .and_then(toml::Value::as_table)
        .ok_or(SpecError::NoMetadata)?;

    let string = |key: &'static str| -> Result<Option<String>, SpecError> {
        match table.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .map(|found| Some(found.to_string()))
                .ok_or(SpecError::MetadataType { key }),
        }
    };

    let version = string("version")?.ok_or(SpecError::NoVersion)?;
    let source = string("source")?.unwrap_or_else(|| PUBLISHED.to_string());
    check_source(&source)?;

    Ok(Metadata {
        source,
        version,
        dockerfile: string("dockerfile")?.unwrap_or_else(|| DEFAULT_DOCKERFILE.to_string()),
        context: string("context")?.unwrap_or_else(|| DEFAULT_CONTEXT.to_string()),
    })
}

/// Applies the environment overrides over `[workspace.metadata.zaino.zebra]`.
fn resolve(
    manifest: &str,
    lock: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Result<ZebraSpec, SpecError> {
    let declared = metadata(manifest)?;
    let get = |key: &str| {
        env.get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };

    let origin = match (get(ENV_GIT), get(ENV_REV)) {
        (Some(url), Some(rev)) => Origin::Git {
            url: url.to_string(),
            rev: rev.to_string(),
        },
        (Some(_), None) => {
            return Err(SpecError::PartialGitOverride {
                present: ENV_GIT,
                missing: ENV_REV,
            });
        }
        (None, Some(_)) => {
            return Err(SpecError::PartialGitOverride {
                present: ENV_REV,
                missing: ENV_GIT,
            });
        }
        (None, None) => {
            let source = get(ENV_SOURCE).map_or(declared.source, str::to_string);
            check_source(&source)?;
            if source == PATCH {
                let (url, rev) = patch_from_lock(lock.ok_or(SpecError::NoLock)?)?;
                Origin::Git { url, rev }
            } else {
                Origin::Published
            }
        }
    };

    Ok(ZebraSpec {
        version: get(ENV_VERSION).map_or(declared.version, str::to_string),
        dockerfile: get(ENV_DOCKERFILE).map_or(declared.dockerfile, str::to_string),
        context: declared.context,
        origin,
    })
}

/// Renders the `zebra!()` macro the test crates invoke.
fn render(spec: &ZebraSpec) -> String {
    let (summary, body) = match &spec.origin {
        Origin::Published => (
            format!("the published `zfnd/zebra:{}` release", spec.version),
            format!("$crate::ztest::Validator::zebrad({:?})", spec.version),
        ),
        Origin::Git { url, rev } => (
            format!("`{url}` at `{rev}`, built on-cluster"),
            format!(
                "$crate::ztest::dev!(\n            \
                 Validator::Zebrad,\n            \
                 git = {:?},\n            \
                 rev = {:?},\n            \
                 dockerfile = {:?},\n            \
                 context = {:?},\n            \
                 version = {:?},\n        )",
                url, rev, spec.dockerfile, spec.context, spec.version
            ),
        ),
    };

    format!(
        "/// The zebrad every live test stands up: {summary}.\n\
         ///\n\
         /// Generated from `[workspace.metadata.zaino.zebra]`; \
         `docs/testing.md` covers pointing it at a fork.\n\
         #[macro_export]\n\
         macro_rules! zebra {{\n    \
         () => {{\n        {body}\n    }};\n}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[workspace]
members = ["zaino-testutils"]

[workspace.metadata.zaino.zebra]
source = "published"
version = "6.2.3"
dockerfile = "docker/Dockerfile"
"#;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn lock_with(packages: &str) -> String {
        format!("version = 4\n{packages}")
    }

    const SHA: &str = "7f985adaa915668264b68ff6d5de7b8d85bff8bc";
    const FORK: &str = "https://github.com/me/zebra";

    #[test]
    fn git_source_branch_patch_yields_the_resolved_commit() {
        let source = format!("git+{FORK}?branch=my-wip#{SHA}");
        assert_eq!(
            parse_git_source(&source).unwrap(),
            (FORK.to_string(), SHA.to_string())
        );
    }

    #[test]
    fn git_source_rev_patch_yields_the_resolved_commit() {
        let source = format!("git+{FORK}?rev={SHA}#{SHA}");
        assert_eq!(
            parse_git_source(&source).unwrap(),
            (FORK.to_string(), SHA.to_string())
        );
    }

    #[test]
    fn git_source_without_a_ref_spec_yields_the_resolved_commit() {
        let source = format!("git+{FORK}#{SHA}");
        assert_eq!(
            parse_git_source(&source).unwrap(),
            (FORK.to_string(), SHA.to_string())
        );
    }

    #[test]
    fn git_source_missing_its_commit_fragment_is_refused() {
        let err = parse_git_source(&format!("git+{FORK}?branch=my-wip")).unwrap_err();
        assert!(matches!(err, SpecError::UnresolvedGitSource(_)), "{err}");
    }

    #[test]
    fn lock_patch_reads_the_commit_off_a_git_sourced_zebra_crate() {
        let lock = lock_with(&format!(
            r#"
[[package]]
name = "zebra-chain"
version = "12.0.0"
source = "git+{FORK}?branch=my-wip#{SHA}"
"#
        ));
        assert_eq!(
            patch_from_lock(&lock).unwrap(),
            (FORK.to_string(), SHA.to_string())
        );
    }

    #[test]
    fn lock_patch_ignores_non_zebra_git_crates() {
        let lock = lock_with(&format!(
            r#"
[[package]]
name = "orchard"
version = "0.11.0"
source = "git+https://github.com/me/orchard?branch=x#{SHA}"
"#
        ));
        let err = patch_from_lock(&lock).unwrap_err();
        assert!(matches!(err, SpecError::NoGitPatch), "{err}");
    }

    #[test]
    fn lock_patch_refuses_an_unpatched_lockfile() {
        let lock = lock_with(
            r#"
[[package]]
name = "zebra-chain"
version = "12.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"
"#,
        );
        let err = patch_from_lock(&lock).unwrap_err();
        assert!(matches!(err, SpecError::NoGitPatch), "{err}");
    }

    #[test]
    fn lock_patch_refuses_a_path_patched_zebra() {
        let lock = lock_with(
            r#"
[[package]]
name = "zebra-chain"
version = "12.0.0"
"#,
        );
        let err = patch_from_lock(&lock).unwrap_err();
        assert!(
            matches!(err, SpecError::PathPatch(ref c) if c == "zebra-chain"),
            "{err}"
        );
    }

    #[test]
    fn lock_patch_refuses_zebra_crates_on_differing_commits() {
        let other = "0000000000000000000000000000000000000000";
        let lock = lock_with(&format!(
            r#"
[[package]]
name = "zebra-chain"
version = "12.0.0"
source = "git+{FORK}?branch=my-wip#{SHA}"

[[package]]
name = "zebra-state"
version = "13.0.0"
source = "git+{FORK}?branch=my-wip#{other}"
"#
        ));
        let err = patch_from_lock(&lock).unwrap_err();
        assert!(matches!(err, SpecError::DivergentPatch { .. }), "{err}");
    }

    #[test]
    fn metadata_reads_the_declared_table() {
        let found = metadata(MANIFEST).unwrap();
        assert_eq!(
            found,
            Metadata {
                source: "published".to_string(),
                version: "6.2.3".to_string(),
                dockerfile: "docker/Dockerfile".to_string(),
                context: ".".to_string(),
            }
        );
    }

    #[test]
    fn metadata_defaults_source_and_paths() {
        let found = metadata("[workspace.metadata.zaino.zebra]\nversion = \"6.2.3\"\n").unwrap();
        assert_eq!(found.source, "published");
        assert_eq!(found.dockerfile, "docker/Dockerfile");
        assert_eq!(found.context, ".");
    }

    #[test]
    fn metadata_refuses_a_missing_version() {
        let err =
            metadata("[workspace.metadata.zaino.zebra]\nsource = \"published\"\n").unwrap_err();
        assert!(matches!(err, SpecError::NoVersion), "{err}");
    }

    #[test]
    fn metadata_refuses_a_missing_table() {
        let err = metadata("[workspace]\nmembers = []\n").unwrap_err();
        assert!(matches!(err, SpecError::NoMetadata), "{err}");
    }

    #[test]
    fn metadata_refuses_an_unknown_source() {
        let manifest = "[workspace.metadata.zaino.zebra]\nversion = \"6.2.3\"\nsource = \"fork\"\n";
        let err = metadata(manifest).unwrap_err();
        assert!(
            matches!(err, SpecError::UnknownSource(ref s) if s == "fork"),
            "{err}"
        );
    }

    #[test]
    fn resolve_defaults_to_the_published_release() {
        let spec = resolve(MANIFEST, None, &env(&[])).unwrap();
        assert_eq!(spec.origin, Origin::Published);
        assert_eq!(spec.version, "6.2.3");
    }

    #[test]
    fn resolve_lets_the_environment_retag_the_published_release() {
        let spec = resolve(MANIFEST, None, &env(&[("ZAINO_ZEBRA_VERSION", "6.3.0")])).unwrap();
        assert_eq!(spec.origin, Origin::Published);
        assert_eq!(spec.version, "6.3.0");
    }

    #[test]
    fn resolve_derives_the_fork_commit_from_the_lockfile() {
        let lock = lock_with(&format!(
            r#"
[[package]]
name = "zebra-chain"
version = "12.0.0"
source = "git+{FORK}?branch=my-wip#{SHA}"
"#
        ));
        let spec = resolve(
            MANIFEST,
            Some(&lock),
            &env(&[("ZAINO_ZEBRA_SOURCE", "patch")]),
        )
        .unwrap();
        assert_eq!(
            spec.origin,
            Origin::Git {
                url: FORK.to_string(),
                rev: SHA.to_string()
            }
        );
        assert_eq!(spec.version, "6.2.3");
    }

    #[test]
    fn resolve_refuses_patch_mode_without_a_lockfile() {
        let err = resolve(MANIFEST, None, &env(&[("ZAINO_ZEBRA_SOURCE", "patch")])).unwrap_err();
        assert!(matches!(err, SpecError::NoLock), "{err}");
    }

    #[test]
    fn resolve_takes_an_explicit_git_override_over_the_lockfile() {
        let spec = resolve(
            MANIFEST,
            None,
            &env(&[("ZAINO_ZEBRA_GIT", FORK), ("ZAINO_ZEBRA_REV", SHA)]),
        )
        .unwrap();
        assert_eq!(
            spec.origin,
            Origin::Git {
                url: FORK.to_string(),
                rev: SHA.to_string()
            }
        );
    }

    #[test]
    fn resolve_refuses_a_half_declared_git_override() {
        let err = resolve(MANIFEST, None, &env(&[("ZAINO_ZEBRA_GIT", FORK)])).unwrap_err();
        assert!(matches!(err, SpecError::PartialGitOverride { .. }), "{err}");
    }

    #[test]
    fn render_published_names_the_release_tag() {
        let spec = ZebraSpec {
            version: "6.2.3".to_string(),
            dockerfile: "docker/Dockerfile".to_string(),
            context: ".".to_string(),
            origin: Origin::Published,
        };
        let out = render(&spec);
        assert!(out.contains(r#"Validator::zebrad("6.2.3")"#), "{out}");
        assert!(!out.contains("dev!"), "{out}");
    }

    #[test]
    fn render_git_names_the_commit_and_the_version() {
        let spec = ZebraSpec {
            version: "6.2.3".to_string(),
            dockerfile: "docker/Dockerfile".to_string(),
            context: ".".to_string(),
            origin: Origin::Git {
                url: FORK.to_string(),
                rev: SHA.to_string(),
            },
        };
        let out = render(&spec);
        assert!(out.contains(&format!(r#"git = "{FORK}""#)), "{out}");
        assert!(out.contains(&format!(r#"rev = "{SHA}""#)), "{out}");
        assert!(out.contains(r#"version = "6.2.3""#), "{out}");
        assert!(out.contains(r#"dockerfile = "docker/Dockerfile""#), "{out}");
    }
}
