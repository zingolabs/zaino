//! Blockcache utility functionality.

/// Parser Error Type.
#[derive(Debug)]
pub enum ParseError {
    Io(std::io::Error),
    InvalidData(String),
}

trait ParseFromSlice {
    fn parse_from_slice(data: &[u8]) -> Result<(&[u8], Self), ParseError>
    where
        Self: Sized;
}
