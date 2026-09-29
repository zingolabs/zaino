//! gRPC-framed `CompactBlock` records + the fixed-width offsets that locate them
//!
//! - record = the exact bytes on the wire (`[0x00][len u32 BE][protobuf]`)
//! - serving a range = one contiguous `blocks.dat` span, no per-record work

/// gRPC length-prefixed message framing: compression flag + big-endian length
pub(crate) const FRAME_HEADER: usize = 5;

/// `offsets.idx` stride: height `h`'s record ends at the u64 LE at `OFFSET * h` (starts where
/// `h - 1`'s ends, 0 for genesis)
pub(crate) const OFFSET: usize = 8;

/// Block hash width (`CompactBlock.hash`)
pub(crate) const HASH: usize = 32;

/// Appends one framed message to `out`: header reserved, `payload` written in place, length patched
pub(crate) fn frame_into<T>(out: &mut Vec<u8>, payload: impl FnOnce(&mut Vec<u8>) -> T) -> T {
    let at = out.len();
    out.extend_from_slice(&[0; FRAME_HEADER]);
    let written = payload(out);
    let len = out.len() - at - FRAME_HEADER;
    let len = u32::try_from(len).expect("record < 4 GiB (a consensus block is <= 2 MB)");
    out[at + 1..at + FRAME_HEADER].copy_from_slice(&len.to_be_bytes());
    written
}

/// Length of the framed record opening `bytes`, header included (`None` = no whole header)
pub(crate) fn framed_len(bytes: &[u8]) -> Option<usize> {
    let header: [u8; 4] = bytes.get(1..FRAME_HEADER)?.try_into().ok()?;
    FRAME_HEADER.checked_add(usize::try_from(u32::from_be_bytes(header)).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_the_grpc_length_prefix_and_concatenate() {
        let mut out = Vec::new();
        frame_into(&mut out, |out| out.extend_from_slice(&[0xaa, 0xbb, 0xcc]));
        frame_into(&mut out, |out| out.push(0xdd));

        let first = [0x00, 0x00, 0x00, 0x00, 0x03, 0xaa, 0xbb, 0xcc];
        let second = [0x00, 0x00, 0x00, 0x00, 0x01, 0xdd];
        assert_eq!(out, [&first[..], &second[..]].concat(), "records back to back = a range");
        assert_eq!(framed_len(&out), Some(first.len()), "first record's own length");
        assert_eq!(framed_len(&out[first.len()..]), Some(second.len()));
        assert_eq!(framed_len(&out[..4]), None, "torn header");
    }
}
