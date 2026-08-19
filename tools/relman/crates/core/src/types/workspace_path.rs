use std::path::{Component, Path};

/// A relative path within the repository working tree.
///
/// Invariants, enforced once at [`parse`](WorkspacePath::parse):
/// - non-empty,
/// - relative (absolute paths are rejected),
/// - no `..` parent-dir traversal.
///
/// The value is stored as a `String` so both [`as_str`](WorkspacePath::as_str)
/// and [`as_path`](WorkspacePath::as_path) hand out borrows without a fallible
/// UTF-8 step — the input was already a `&str`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspacePath(String);

/// Why a string was rejected as a [`WorkspacePath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidWorkspacePath {
    /// The input was the empty string.
    #[error("workspace path is empty")]
    Empty,
    /// The path was absolute (had a root or drive prefix).
    #[error("workspace path must be relative, found absolute path {0:?}")]
    Absolute(String),
    /// The path contained a `..` component.
    #[error("workspace path must not contain '..' traversal, found {0:?}")]
    Traversal(String),
}

impl WorkspacePath {
    /// Parse a relative workspace path, enforcing the documented invariants.
    pub fn parse(raw: &str) -> Result<Self, InvalidWorkspacePath> {
        if raw.is_empty() {
            return Err(InvalidWorkspacePath::Empty);
        }
        let path = Path::new(raw);
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    return Err(InvalidWorkspacePath::Traversal(raw.to_owned()));
                }
                Component::Prefix(_) | Component::RootDir => {
                    return Err(InvalidWorkspacePath::Absolute(raw.to_owned()));
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the path as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Borrow the path as a [`Path`].
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_relative_paths() {
        for raw in ["packages/zaino-state", "Cargo.toml", ".changesets", "a/b/c.md"] {
            let parsed = WorkspacePath::parse(raw).expect("should be valid");
            assert_eq!(parsed.as_str(), raw);
            assert_eq!(parsed.as_path(), Path::new(raw));
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(WorkspacePath::parse(""), Err(InvalidWorkspacePath::Empty));
    }

    #[test]
    fn rejects_absolute() {
        assert_eq!(
            WorkspacePath::parse("/etc/passwd"),
            Err(InvalidWorkspacePath::Absolute("/etc/passwd".to_owned()))
        );
    }

    #[test]
    fn rejects_parent_traversal() {
        assert_eq!(
            WorkspacePath::parse("../secrets"),
            Err(InvalidWorkspacePath::Traversal("../secrets".to_owned()))
        );
        assert_eq!(
            WorkspacePath::parse("packages/../../escape"),
            Err(InvalidWorkspacePath::Traversal(
                "packages/../../escape".to_owned()
            ))
        );
    }
}
