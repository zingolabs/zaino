//! gRPC length-prefixed message framing: `[0x00][len u32 BE][message]`
//!
//! - compression flag always 0 (no compression negotiated)

/// Compression flag + big-endian length
pub const FRAME_HEADER: usize = 5;

/// Appends one framed message to `out`: header reserved, `payload` written in place, length patched
pub fn frame_into<T>(out: &mut Vec<u8>, payload: impl FnOnce(&mut Vec<u8>) -> T) -> T {
    let at = out.len();
    out.extend_from_slice(&[0; FRAME_HEADER]);
    let written = payload(out);
    let len = u32::try_from(out.len() - at - FRAME_HEADER)
        .expect("gRPC message < 4 GiB (a consensus block is <= 2 MB)");
    out[at + 1..at + FRAME_HEADER].copy_from_slice(&len.to_be_bytes());
    written
}

/// Length of the frame opening `bytes`, header included (`None` = no whole header)
pub fn framed_len(bytes: &[u8]) -> Option<usize> {
    let header: [u8; 4] = bytes.get(1..FRAME_HEADER)?.try_into().ok()?;
    FRAME_HEADER.checked_add(usize::try_from(u32::from_be_bytes(header)).ok()?)
}

/// `(message, rest)` of the frame opening `bytes` (`None` = torn header or message)
pub fn split_frame(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (frame, rest) = bytes.split_at_checked(framed_len(bytes)?)?;
    Some((&frame[FRAME_HEADER..], rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_the_grpc_length_prefix_and_split_back_apart() {
        let mut out = Vec::new();
        frame_into(&mut out, |out| out.extend_from_slice(&[0xaa, 0xbb, 0xcc]));
        frame_into(&mut out, |_| ());
        frame_into(&mut out, |out| out.push(0xdd));

        let first = [0x00, 0x00, 0x00, 0x00, 0x03, 0xaa, 0xbb, 0xcc];
        let empty = [0x00, 0x00, 0x00, 0x00, 0x00];
        let last = [0x00, 0x00, 0x00, 0x00, 0x01, 0xdd];
        assert_eq!(out, [&first[..], &empty[..], &last[..]].concat(), "frames back to back");

        assert_eq!(framed_len(&out), Some(first.len()), "first frame's own length");
        assert_eq!(framed_len(&out[..4]), None, "torn header");

        let mut messages = Vec::new();
        let mut rest = &out[..];
        while let Some((message, tail)) = split_frame(rest) {
            messages.push(message);
            rest = tail;
        }
        assert_eq!(messages, [&[0xaa, 0xbb, 0xcc][..], &[], &[0xdd]]);
        assert!(rest.is_empty(), "every byte walked");

        assert_eq!(split_frame(&first[..7]), None, "torn message");
        assert_eq!(split_frame(&first[..4]), None, "torn header");
    }
}
