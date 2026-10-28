//! Unique identifier for a registered index.

/// Unique identifier for a registered index.
///
/// Newtype over `&'static str` — prevents accidental use of arbitrary
/// strings where an index name is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IndexId(&'static str);

impl IndexId {
    /// Create an index identifier from a static string.
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The string value.
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl core::fmt::Display for IndexId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}
