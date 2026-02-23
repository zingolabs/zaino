//! Metadata objects

use core2::io::{self, Read, Write};

use crate::{read_u64_le, version, write_u64_le, FixedEncodedLen, ZainoVersionedSerde};

/// Holds information about the mempool state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MempoolInfo {
    /// Current tx count
    pub size: u64,
    /// Sum of all tx sizes
    pub bytes: u64,
    /// Total memory usage for the mempool
    pub usage: u64,
}

impl ZainoVersionedSerde for MempoolInfo {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u64_le(&mut w, self.size)?;
        write_u64_le(&mut w, self.bytes)?;
        write_u64_le(&mut w, self.usage)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let size = read_u64_le(&mut r)?;
        let bytes = read_u64_le(&mut r)?;
        let usage = read_u64_le(&mut r)?;
        Ok(MempoolInfo { size, bytes, usage })
    }
}

/// 24 byte body.
impl FixedEncodedLen for MempoolInfo {
    /// 8 byte size + 8 byte bytes + 8 byte usage
    const ENCODED_LEN: usize = 8 + 8 + 8;
}

impl From<zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse> for MempoolInfo {
    fn from(resp: zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse) -> Self {
        MempoolInfo {
            size: resp.size,
            bytes: resp.bytes,
            usage: resp.usage,
        }
    }
}

impl From<MempoolInfo> for zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse {
    fn from(info: MempoolInfo) -> Self {
        zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse {
            size: info.size,
            bytes: info.bytes,
            usage: info.usage,
        }
    }
}
