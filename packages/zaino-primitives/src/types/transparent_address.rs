//! Transparent Zcash address.

/// A transparent Zcash address (t-addr string).
///
/// Wraps the string representation. Validation of address format
/// is the adapter's responsibility at construction time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransparentAddress(String);

impl TransparentAddress {
    /// Wrap a validated address string.
    pub fn new(address: String) -> Self {
        Self(address)
    }

    /// The address string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for TransparentAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<TransparentAddress> for String {
    fn from(a: TransparentAddress) -> Self {
        a.0
    }
}
