use std::path::PathBuf;

use super::*;
use crate::pages::PageError;

/// Of several merges failing at once, the write reports the gravest: corrupt input over a fatal
/// failure over a transient one (a transient failure never hides one that must stop writes)
#[test]
fn the_gravest_of_several_merge_failures_is_kept() {
    let transient = SegmentError::Io(std::io::Error::other("EIO"));
    let fatal = SegmentError::Sync(std::io::Error::other("EIO"));
    let corrupt = SegmentError::Page(PageError::Corrupt {
        path: PathBuf::from("/x"),
        page: 0,
    });
    assert!(severity(&corrupt) > severity(&fatal));
    assert!(severity(&fatal) > severity(&transient));
}
