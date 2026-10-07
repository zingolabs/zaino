//! One exact consensus header (`HeaderBytes` with nothing after the solution), its bytes kept for
//! the Equihash check

use zaino_primitives::types::{BlockHash, HeaderBytes, HeaderError, MerkleRoot};

#[derive(Clone, PartialEq, Eq)]
pub struct Header {
    hash: BlockHash,
    bytes: Box<[u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error(transparent)]
    Header(#[from] HeaderError),
    #[error("{trailing} bytes after the solution")]
    Trailing { trailing: usize },
}

/// Raw consensus bytes → [`Header`] (one exact header: nothing after the solution)
pub fn decode_header(raw: &[u8]) -> Result<Header, DecodeError> {
    let (header, rest) = HeaderBytes::split(raw)?;
    if !rest.is_empty() {
        return Err(DecodeError::Trailing { trailing: rest.len() });
    }
    Ok(Header { hash: header.hash(), bytes: raw.into() })
}

impl Header {
    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    fn view(&self) -> HeaderBytes<'_> {
        let (header, _) =
            HeaderBytes::split(&self.bytes).expect("decode_header checked the layout");
        header
    }

    pub(crate) fn version(&self) -> i32 {
        self.view().version()
    }

    pub(crate) fn prev_hash(&self) -> BlockHash {
        self.view().prev_hash()
    }

    pub(crate) fn merkle_root(&self) -> MerkleRoot {
        self.view().merkle_root()
    }

    pub(crate) fn time(&self) -> u32 {
        self.view().time()
    }

    pub(crate) fn bits(&self) -> u32 {
        self.view().bits()
    }

    /// Consensus bytes as received
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn equihash_input(&self) -> &[u8] {
        self.view().equihash_input()
    }

    pub(crate) fn nonce(&self) -> [u8; 32] {
        self.view().nonce()
    }

    pub(crate) fn solution(&self) -> &[u8] {
        self.view().solution()
    }
}

impl std::fmt::Debug for Header {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Header")
            .field("hash", &self.hash)
            .field("prev_hash", &self.prev_hash())
            .field("time", &self.time())
            .field("bits", &format_args!("{:#010x}", self.bits()))
            .finish()
    }
}
