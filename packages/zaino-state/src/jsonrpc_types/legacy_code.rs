/// The zcashd JSON-RPC error codes Zaino reports, numbered as zcashd's `src/rpc/protocol.h` numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyCode {
    /// `std::exception` thrown in command handling.
    Misc = -1,
    /// Invalid address or key.
    InvalidAddressOrKey = -5,
    /// Invalid, missing or duplicate parameter.
    InvalidParameter = -8,
}

impl From<LegacyCode> for i32 {
    fn from(code: LegacyCode) -> Self {
        code as i32
    }
}
