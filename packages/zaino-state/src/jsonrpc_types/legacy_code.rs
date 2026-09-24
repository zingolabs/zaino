/// The zcashd JSON-RPC error codes, numbered as zcashd's `src/rpc/protocol.h` numbers them.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyCode {
    /// `std::exception` thrown in command handling.
    #[default]
    Misc = -1,
    /// Server is in safe mode, and command is not allowed in safe mode.
    ForbiddenBySafeMode = -2,
    /// Unexpected type was passed as parameter.
    Type = -3,
    /// Invalid address or key.
    InvalidAddressOrKey = -5,
    /// Ran out of memory during operation.
    OutOfMemory = -7,
    /// Invalid, missing or duplicate parameter.
    InvalidParameter = -8,
    /// Database error.
    Database = -20,
    /// Error parsing or validating structure in raw format.
    Deserialization = -22,
    /// General error during transaction or block submission.
    Verify = -25,
    /// Transaction or block was rejected by network rules.
    VerifyRejected = -26,
    /// Transaction already in chain.
    VerifyAlreadyInChain = -27,
    /// Client still warming up.
    InWarmup = -28,
    /// Bitcoin is not connected.
    ClientNotConnected = -9,
    /// Still downloading initial blocks.
    ClientInInitialDownload = -10,
    /// Node is already added.
    ClientNodeAlreadyAdded = -23,
    /// Node has not been added before.
    ClientNodeNotAdded = -24,
    /// Node to disconnect not found in connected nodes.
    ClientNodeNotConnected = -29,
    /// Invalid IP/Subnet.
    ClientInvalidIpOrSubnet = -30,
}

impl From<LegacyCode> for i32 {
    fn from(code: LegacyCode) -> Self {
        code as i32
    }
}
