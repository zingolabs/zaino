//! Whether one device flush can make an index directory's commits durable ([`Durability::Barrier`])
//!
//! - sequence commit = overwrite of zeroed reserve blocks → no metadata → fsync = cache flush only
//! - flush = every completed write durable, any file (kernel `block/writeback_cache_control.rst`)
//! - anything but ext4 / XFS = `PerFile` (CoW, log-structured, network: overwrite = new metadata)
//! - allowlist on positive evidence: any unknown fact, or any probe failing = `PerFile`
//! - ext4 mount: `barrier`, no `journal_async_commit`, no DAX (`/proc/fs/ext4/<dev>/options`
//!   lists every option incl. superblock defaults, mountinfo hides them)
//! - XFS mount: one device (no `logdev` / `rtdev`), no DAX
//! - every file the barrier covers: on the first's device, not DAX, no shared (reflinked) extent
//!   on XFS (overwriting one = copy-on-write: a remap the barrier never commits)
//! - re-verify on a kernel bump: `ext4_sync_file`, `xfs_file_fsync` still flush a clean file's
//!   device, option names above unchanged

use std::{
    io,
    path::{Path, PathBuf},
};

use rustix::fs::{AtFlags, StatxAttributes, StatxFlags, CWD};

use super::Durability;

/// `files` = every file a `Barrier` commit writes back (none = `PerFile`)
pub(crate) fn durability(files: &[PathBuf]) -> Durability {
    match barrier_safe(files) {
        Ok(true) => Durability::Barrier,
        Ok(false) | Err(_) => Durability::PerFile,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filesystem {
    Ext4,
    Xfs,
}

fn barrier_safe(files: &[PathBuf]) -> io::Result<bool> {
    let Some(first) = files.first() else { return Ok(false) };
    let at = stat(first)?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    let Some(mount) = at.mount.and_then(|id| mount(&mountinfo, id)) else { return Ok(false) };
    let (filesystem, options) = match mount.fstype.as_str() {
        "ext4" => (Filesystem::Ext4, ext4_options(mount.dev)?),
        "xfs" => (Filesystem::Xfs, mount.options),
        _ => return Ok(false),
    };
    Ok(mount_allows(filesystem, &options) && files_in_place(files, filesystem, at.dev)?)
}

/// One mountinfo record: device, filesystem type, super options (escapes undone)
#[derive(Debug, PartialEq, Eq)]
struct Mount {
    dev: (u32, u32),
    fstype: String,
    options: Vec<String>,
}

/// Mount `id`'s record in `/proc/self/mountinfo` text (`proc_pid_mountinfo(5)`)
///
/// - `id maj:min root point mount-opts [optional…] - fstype source super-opts`
fn mount(mountinfo: &str, id: u64) -> Option<Mount> {
    mountinfo.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.first()?.parse::<u64>().ok()? != id {
            return None;
        }
        let separator = 6 + fields.get(6..)?.iter().position(|field| *field == "-")?;
        let (major, minor) = fields.get(2)?.split_once(':')?;
        Some(Mount {
            dev: (major.parse().ok()?, minor.parse().ok()?),
            fstype: unescape(fields.get(separator + 1)?),
            options: fields.get(separator + 3)?.split(',').map(unescape).collect(),
        })
    })
}

/// `\ooo` octal escapes undone (space, tab, newline, backslash, `,`, `=`)
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let octal = bytes.get(at + 1..at + 4).filter(|digits| {
            bytes[at] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });
        match octal {
            Some(digits) => {
                out.push(digits.iter().fold(0u8, |value, digit| value * 8 + (digit - b'0')));
                at += 4;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every option the ext4 on device `dev` runs with, defaults included
fn ext4_options(dev: (u32, u32)) -> io::Result<Vec<String>> {
    let partitions = std::fs::read_to_string("/proc/partitions")?;
    let name = device_name(&partitions, dev).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("device {}:{} unlisted", dev.0, dev.1))
    })?;
    let options = std::fs::read_to_string(format!("/proc/fs/ext4/{name}/options"))?;
    Ok(options.lines().map(str::to_owned).collect())
}

/// Device `dev`'s name in `/proc/partitions` text (`major minor #blocks name`)
fn device_name(partitions: &str, dev: (u32, u32)) -> Option<&str> {
    partitions.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let major: u32 = fields.next()?.parse().ok()?;
        let minor: u32 = fields.next()?.parse().ok()?;
        ((major, minor) == dev).then_some(fields.nth(1)?)
    })
}

/// Mount options under which one flush covers every overwrite in place
fn mount_allows(filesystem: Filesystem, options: &[String]) -> bool {
    let has = |wanted: &str| options.iter().any(|option| option == wanted);
    let dax = has("dax") || has("dax=always");
    match filesystem {
        Filesystem::Ext4 => has("barrier") && !has("journal_async_commit") && !dax,
        Filesystem::Xfs => {
            let other_device =
                |option: &String| option.starts_with("logdev=") || option.starts_with("rtdev=");
            !dax && !options.iter().any(other_device)
        }
    }
}

/// `statx` facts the probe reads (`mount` = `None`: kernel < 5.8 reports no mount id)
struct Stat {
    mount: Option<u64>,
    dev: (u32, u32),
    dax: bool,
}

fn stat(path: &Path) -> io::Result<Stat> {
    let got = rustix::fs::statx(CWD, path, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::MNT_ID)?;
    let has_mount = StatxFlags::from_bits_retain(got.stx_mask).contains(StatxFlags::MNT_ID);
    let dax = got.stx_attributes_mask.contains(StatxAttributes::DAX)
        && got.stx_attributes.contains(StatxAttributes::DAX);
    Ok(Stat {
        mount: has_mount.then_some(got.stx_mnt_id),
        dev: (got.stx_dev_major, got.stx_dev_minor),
        dax,
    })
}

/// Every one of `files` on device `dev`, none DAX, none sharing an extent (XFS)
fn files_in_place(files: &[PathBuf], filesystem: Filesystem, dev: (u32, u32)) -> io::Result<bool> {
    for path in files {
        let at = stat(path)?;
        if at.dev != dev || at.dax {
            return Ok(false);
        }
        if filesystem == Filesystem::Xfs && shares_extents(&std::fs::File::open(path)?)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `FS_IOC_FIEMAP` (`Documentation/filesystems/fiemap.rst`): any extent flagged
/// `FIEMAP_EXTENT_SHARED` (no FIEMAP items in libc / rustix: the uapi layout, 32 extents a call)
#[allow(unsafe_code)]
fn shares_extents(file: &std::fs::File) -> io::Result<bool> {
    use rustix::ioctl::{ioctl, opcode, Opcode, Updater};

    const FS_IOC_FIEMAP: Opcode = opcode::read_write::<Header>(b'f', 11);
    // uapi value (`Header` = 32 B)
    const _: () = assert!(FS_IOC_FIEMAP == 0xC020_660B);
    const FIEMAP_EXTENT_LAST: u32 = 0x1;
    const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
    const EXTENTS: usize = 32;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    /// uapi `struct fiemap` minus its flexible extent array (= the `_IOWR` size)
    #[repr(C)]
    struct Header {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
    }

    #[repr(C)]
    struct Request {
        header: Header,
        extents: [Extent; EXTENTS],
    }

    let mut start = 0;
    loop {
        let mut request = Request {
            header: Header {
                start,
                length: u64::MAX - start,
                flags: 0,
                mapped: 0,
                count: u32::try_from(EXTENTS).expect("32 extents fit u32"),
                reserved: 0,
            },
            extents: [Extent::default(); EXTENTS],
        };
        // sound: `Request` = `struct fiemap` + room for `count` extents, borrowed for the call
        unsafe { ioctl(file, Updater::<FS_IOC_FIEMAP, Request>::new(&mut request)) }?;
        let mapped = usize::try_from(request.header.mapped).expect("u32 fits usize (64-bit)");
        let mapped = &request.extents[..mapped.min(EXTENTS)];
        if mapped.iter().any(|extent| extent.flags & FIEMAP_EXTENT_SHARED != 0) {
            return Ok(true);
        }
        match mapped.last() {
            Some(last) if last.flags & FIEMAP_EXTENT_LAST == 0 => {
                start = last.logical + last.length;
            }
            _ => return Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(list: &str) -> Vec<String> {
        list.split(',').map(str::to_owned).collect()
    }

    /// - real mountinfo lines: optional fields before ` - `, none, `\040` in a mount point, `fuse.`
    /// - id picks the line, escapes undone
    #[test]
    fn mountinfo_records_parse_by_mount_id() {
        let mountinfo = "\
22 1 259:2 /@ / rw,noatime shared:1 - btrfs /dev/nvme1n1p2 rw,compress=zstd:3,subvol=/@
1201 1188 254:6 / /app/data rw,relatime - ext4 /dev/mapper/ztest-32eb rw,stripe=16
1300 22 0:52 / /mnt/my\\040disk rw master:5 shared:9 - fuse.sshfs host:/x rw,user_id=0
";
        let parsed = |id| mount(mountinfo, id);
        assert_eq!(
            parsed(1201),
            Some(Mount { dev: (254, 6), fstype: "ext4".into(), options: options("rw,stripe=16") })
        );
        let btrfs = parsed(22).expect("btrfs line");
        assert_eq!((btrfs.fstype.as_str(), btrfs.dev), ("btrfs", (259, 2)));
        assert_eq!(parsed(1300).map(|m| m.fstype), Some("fuse.sshfs".to_owned()));
        assert_eq!(parsed(7), None, "no such mount");
        assert_eq!(unescape("a\\040b\\134c\\054d"), "a b\\c,d");
        assert_eq!(unescape("trailing\\04"), "trailing\\04", "short escape kept verbatim");
    }

    /// `/proc/partitions` maps `maj:min` to the name `/proc/fs/ext4` uses
    #[test]
    fn partitions_name_a_device_by_its_numbers() {
        let partitions = "major minor  #blocks  name\n\n 259        2 3905968128 nvme1n1p2\n \
                          254        6  167772160 dm-6\n";
        assert_eq!(device_name(partitions, (254, 6)), Some("dm-6"));
        assert_eq!(device_name(partitions, (254, 7)), None);
    }

    /// - ext4 allowed: barrier on, no async commit, no DAX
    /// - XFS allowed: one device, no DAX
    /// - every other combination refused
    #[test]
    fn only_flush_honouring_in_place_mounts_allow_a_barrier() {
        #[rustfmt::skip]
        let cases = [
            (Filesystem::Ext4, "rw,barrier,data=ordered,commit=5",                true),
            (Filesystem::Ext4, "rw,barrier,data=journal",                         true),
            (Filesystem::Ext4, "rw,nobarrier,data=ordered",                       false),
            (Filesystem::Ext4, "rw,data=ordered",                                 false),
            (Filesystem::Ext4, "rw,barrier,journal_async_commit,data=ordered",    false),
            (Filesystem::Ext4, "rw,barrier,dax=always",                           false),
            (Filesystem::Ext4, "rw,barrier,dax",                                  false),
            (Filesystem::Ext4, "rw,barrier,dax=inode",                            true),
            (Filesystem::Xfs,  "rw,relatime,attr2,inode64,noquota",               true),
            (Filesystem::Xfs,  "rw,attr2,dax=always",                             false),
            (Filesystem::Xfs,  "rw,logdev=/dev/sdb1,noquota",                     false),
            (Filesystem::Xfs,  "rw,rtdev=/dev/sdc1",                              false),
        ];
        for (filesystem, list, allowed) in cases {
            assert_eq!(mount_allows(filesystem, &options(list)), allowed, "{filesystem:?} {list}");
        }
    }

    /// Real probe end to end: `statx` mount id → this process's mountinfo → `tmpfs` (not ext4 /
    /// XFS)
    #[test]
    fn a_file_on_tmpfs_commits_per_file() {
        let file = tempfile::NamedTempFile::new_in("/dev/shm").expect("temp file on /dev/shm");

        let probed = durability(&[file.path().to_path_buf()]);

        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").expect("mountinfo");
        let at = stat(file.path()).expect("statx");
        let fstype = at.mount.and_then(|id| mount(&mountinfo, id)).map(|mount| mount.fstype);
        assert_eq!(fstype.as_deref(), Some("tmpfs"), "statx mount id → its mountinfo line");
        assert_eq!(probed, Durability::PerFile, "{} on tmpfs", file.path().display());
    }

    /// - no files = nothing a barrier covers
    /// - failed probe (`statx` on a missing file) = `PerFile`, never an error
    #[test]
    fn no_files_or_a_failed_probe_commit_per_file() {
        let missing = PathBuf::from("/nonexistent/zaino-probe/file");

        assert_eq!(durability(&[]), Durability::PerFile, "no files");
        assert_eq!(durability(&[missing]), Durability::PerFile, "a missing file");
    }
}
