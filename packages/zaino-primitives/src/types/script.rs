//! Transaction output script.

/// A transparent output script (raw bytes).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Script(Vec<u8>);

impl Script {
    /// Wrap raw script bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<Script> for Vec<u8> {
    fn from(s: Script) -> Self {
        s.0
    }
}

impl From<Vec<u8>> for Script {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}
