use super::*;
use crate::fs::SimFs;

fn identity() -> Identity {
    Identity::new("compact_block", 1).expect("short name")
}

/// A directory several missing levels deep, and a nested subdirectory, survive a power loss
/// right after the first commit: every new level durably linked in its parent
#[test]
fn every_new_directory_level_survives_a_power_loss() {
    let fs = SimFs::new();
    let path = Path::new("/a/b/c/idx");
    let mut opened = IndexDir::open(fs.clone(), path, identity()).expect("open fresh");
    let nested = opened.dir.subdir("p/q").expect("subdir");
    opened.dir.commit(&[7]).expect("commit");
    drop(opened);

    let crashed = fs.power_loss();
    assert!(crashed.list(&nested).is_ok(), "nested subdirectory kept");
    let reopened = IndexDir::open(crashed, path, identity()).expect("reopen");
    assert_eq!(reopened.body, Some(vec![7]), "the commit survived");

    // a relative single name: its entry synced in the current directory
    let fs = SimFs::new();
    let relative = Path::new("idx");
    let mut opened = IndexDir::open(fs.clone(), relative, identity()).expect("open fresh");
    opened.dir.commit(&[8]).expect("commit");
    drop(opened);
    let reopened = IndexDir::open(fs.power_loss(), relative, identity()).expect("reopen");
    assert_eq!(
        reopened.body,
        Some(vec![8]),
        "the relative directory survived"
    );
}

/// - fresh → commit → reopen reads it back; commits after a reopen continue the sequence
///   (never over the live slot); manifest never past its two slots
/// - second opener locked out; foreign index never committed state
#[test]
fn commits_survive_reopen_the_lock_excludes_and_identity_is_enforced() {
    let fs = SimFs::new();
    let path = Path::new("/data/idx");

    let mut opened = IndexDir::open(fs.clone(), path, identity()).expect("open fresh");
    assert_eq!(opened.body, None);
    let manifest_len = || fs.contents(&path.join(MANIFEST)).expect("manifest").len() as u64;
    assert_eq!(
        manifest_len(),
        manifest::FILE_LEN,
        "both slots exist before any commit"
    );
    opened
        .dir
        .ensure_empty("blocks.dat")
        .expect("absent = empty");
    opened.dir.commit(&[1, 2, 3]).expect("commit");

    let locked = IndexDir::open(fs.clone(), path, identity()).expect_err("locked");
    let would_block =
        matches!(&locked, ManifestError::Io(e) if e.kind() == io::ErrorKind::WouldBlock);
    assert!(would_block, "{locked}");

    fs.open(&path.join("blocks.dat"))
        .expect("file")
        .write_all_at(&[9], 0)
        .expect("write");
    let written = opened.dir.ensure_empty("blocks.dat");
    assert!(matches!(written, Err(ManifestError::Unmanifested { .. })));
    drop(opened);

    for (body, then) in [([1, 2, 3], [4, 5, 6]), ([4, 5, 6], [7, 8, 9])] {
        let mut reopened = IndexDir::open(fs.clone(), path, identity()).expect("reopen");
        assert_eq!(reopened.body.as_deref(), Some(&body[..]));
        reopened.dir.commit(&then).expect("commit after reopen");
    }
    let reopened = IndexDir::open(fs.clone(), path, identity()).expect("reopen");
    assert_eq!(
        reopened.body.as_deref(),
        Some(&[7, 8, 9][..]),
        "the newest of three"
    );
    assert_eq!(
        manifest_len(),
        manifest::FILE_LEN,
        "every commit overwrote a slot in place"
    );
    drop(reopened);

    let foreign = Identity::new("tree_state", 1).expect("short name");
    let foreign = IndexDir::open(fs, path, foreign);
    assert!(matches!(foreign, Err(ManifestError::Name { .. })));
}

/// A body past the slot: `Full`, the committed state unchanged, later commits continue
#[test]
fn a_commit_past_the_slot_is_refused_and_nothing_moves() {
    let fs = SimFs::new();
    let path = Path::new("/idx");
    let mut opened = IndexDir::open(fs.clone(), path, identity()).expect("open");
    opened.dir.commit(&[1]).expect("commit");
    let full = opened.dir.commit(&vec![0; manifest::SLOT]);
    assert!(matches!(full, Err(ManifestError::Full { .. })), "{full:?}");
    opened.dir.commit(&[2]).expect("commit after a refusal");
    drop(opened);
    let reopened = IndexDir::open(fs, path, identity()).expect("reopen");
    assert_eq!(reopened.body.as_deref(), Some(&[2][..]));
}

/// - every crash state of a commit reopens to the old body or the new one, never neither
/// - commit 3 = first overwrite of a used slot (commit 1's)
#[test]
fn a_commit_is_atomic_under_every_crash_state() {
    let fs = SimFs::recording();
    let path = Path::new("/idx");
    let mut opened = IndexDir::open(fs.clone(), path, identity()).expect("open");
    for commit in 1u8..=3 {
        opened.dir.commit(&[commit]).expect("commit");
        fs.set_tag(u64::from(commit));
    }
    drop(opened);

    let states = fs.crash_states();
    assert!(
        states.len() > 10,
        "enumerated {} crash states",
        states.len()
    );
    for state in states {
        let body = IndexDir::open(state.fs, path, identity())
            .unwrap_or_else(|error| panic!("{}: {error}", state.label))
            .body;
        let acknowledged = u8::try_from(state.tag).expect("tag = commits");
        let allowed = [acknowledged, acknowledged + 1]
            .map(|commit| (commit > 0).then(|| vec![commit.min(3)]));
        assert!(allowed.contains(&body), "{}: {body:?}", state.label);
    }
}

/// - offline reader (verify, read only, no lock) = what `IndexDir` sees
/// - nothing before a manifest or its first commit, then the newest commit
/// - older layout = `InvalidData`
#[test]
fn the_offline_reader_answers_the_newest_commit() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("idx");
    let fs = crate::fs::RealFs::shared();
    let read = || manifest::read(fs.as_ref(), &path, &identity());
    assert_eq!(read().expect("no directory"), None);

    let mut opened = IndexDir::open(fs.clone(), &path, identity())
        .expect("open")
        .dir;
    assert_eq!(
        read().expect("zeroed slots"),
        None,
        "created, never committed"
    );
    for body in [[1u8], [2], [3]] {
        opened.commit(&body).expect("commit");
        assert_eq!(read().expect("committed"), Some(body.to_vec()));
    }
    drop(opened);

    std::fs::write(path.join(MANIFEST), [1; 40]).expect("older layout");
    let older = read().expect_err("refused");
    assert!(
        matches!(older, ManifestError::Layout { len: 40 }),
        "refused by its length, before any read: {older}"
    );
}

/// Every file counted, nested included; missing directory = error
#[test]
fn disk_bytes_sums_every_file_under_the_directory() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(root.path().join("MANIFEST"), [0; 10]).expect("manifest");
    std::fs::create_dir(root.path().join("by_hash")).expect("subdir");
    std::fs::write(root.path().join("by_hash/0.seg"), [0; 4096]).expect("segment");
    std::fs::write(root.path().join("by_hash/1.seg"), []).expect("empty segment");

    assert_eq!(disk_bytes(root.path()).expect("walk"), 4106);
    let missing = disk_bytes(&root.path().join("absent")).expect_err("absent");
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
}
