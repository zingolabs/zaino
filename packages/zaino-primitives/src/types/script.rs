//! Transaction output script, and how it is classified.

/// The standard forms a transparent output script can take.
///
/// A classification, not an encoding. An index that keys outputs by address
/// needs to know which of the two standard forms it is looking at, because the
/// 20 bytes mean different things in each — a public-key hash in one, a script
/// hash in the other — and a non-standard script has no such hash at all.
///
/// Deliberately carries no discriminants. The on-disk tag values belong to
/// whichever backend writes them, so that a second backend is free to choose
/// its own without the vocabulary crate having already decided. A backend maps
/// this to and from its own tags at its persistence boundary.
///
/// `NonStandard` is a real answer, not a failure: such outputs exist on chain
/// and an index must decide what to do with them. What it decides is the
/// index's business — Zaino's transparent-address history keys them, while its
/// UTXO-set accumulator excludes them, mirroring zcashd's `IsUnspendable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScriptType {
    /// Pay-to-public-key-hash — a `t1...` address.
    P2PKH,
    /// Pay-to-script-hash — a `t3...` address.
    P2SH,
    /// Anything else.
    NonStandard,
}

impl ScriptType {
    /// The classification's name, for diagnostics and wire responses.
    pub fn as_str(&self) -> &'static str {
        match self {
            ScriptType::P2PKH => "P2PKH",
            ScriptType::P2SH => "P2SH",
            ScriptType::NonStandard => "NonStandard",
        }
    }
}

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
