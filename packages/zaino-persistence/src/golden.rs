//! Golden-bytes assertion for every on-disk layout test

/// `encoded` = `fields` back to back, nothing after
///
/// - failure names the field + its byte range (never a flat diff to count through by hand)
pub(crate) fn assert_fields(encoded: &[u8], fields: &[(&str, &[u8])]) {
    let mut at = 0;
    for (field, golden) in fields {
        let end = at + golden.len();
        assert_eq!(encoded.get(at..end), Some(*golden), "{field}, bytes {at}..{end}");
        at = end;
    }
    assert_eq!(encoded.len(), at, "trailing bytes after the last field: {:?}", &encoded[at..]);
}
