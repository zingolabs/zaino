//! Transparent output script (raw bytes)

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Script(Vec<u8>);

impl Script {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn heap_size(&self) -> usize {
        self.0.capacity()
    }
}

impl From<Script> for Vec<u8> {
    fn from(s: Script) -> Self {
        s.0
    }
}
