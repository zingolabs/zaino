//! Note commitment: a shielded note's leaf in the commitment tree

/// Sapling `cmu` / Orchard `cmx`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NoteCommitment([u8; 32]);

impl From<[u8; 32]> for NoteCommitment {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<NoteCommitment> for [u8; 32] {
    fn from(n: NoteCommitment) -> Self {
        n.0
    }
}
