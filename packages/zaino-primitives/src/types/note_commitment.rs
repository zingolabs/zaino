//! Note commitment: a shielded note's leaf in the commitment tree

/// Sapling `cmu` / Orchard `cmx`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NoteCommitment(pub [u8; 32]);
