//! Read the data disk's ext4 geometry on the host and grow it in the Linux guest.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::fs::FileExt;

pub(crate) struct Geometry {
    pub bytes: u64,
    pub block_size: u64,
}

pub(crate) fn geometry(device: &File) -> io::Result<Geometry> {
    let mut superblock = [0u8; 1024];
    device.read_exact_at(&mut superblock, 1024)?;
    let invalid = || io::Error::new(ErrorKind::InvalidData, "invalid ext4 superblock");
    let field = |offset| u32::from_le_bytes(superblock[offset..offset + 4].try_into().unwrap());
    if superblock[0x38..0x3a] != [0x53, 0xef] || field(0x18) > 6 {
        return Err(invalid());
    }
    let block_size = 1024 << field(0x18);
    let mut blocks = u64::from(field(4));
    if field(0x60) & 0x80 != 0 {
        // EXT4_FEATURE_INCOMPAT_64BIT
        blocks |= u64::from(field(0x150)) << 32;
    }
    let bytes = blocks
        .checked_mul(block_size)
        .filter(|&bytes| bytes > 0)
        .ok_or_else(invalid)?;
    Ok(Geometry { bytes, block_size })
}

#[cfg(target_os = "linux")]
pub(crate) fn grow_mounted(
    device: &File,
    mount: &File,
    device_bytes: u64,
) -> io::Result<Option<(u64, u64)>> {
    use std::os::fd::AsRawFd;
    let before = geometry(device)?;
    let target_blocks = device_bytes / before.block_size;
    let target_bytes = target_blocks * before.block_size;
    if before.bytes > target_bytes {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "the ext4 filesystem is larger than its disk",
        ));
    }
    if before.bytes == target_bytes {
        return Ok(None);
    }

    // EXT4_IOC_RESIZE_FS = _IOW('f', 16, __u64) on the VM's arm64 and x86_64
    // Linux guests. The kernel grows the mounted filesystem through its journal.
    const RESIZE_FS: u32 = 0x4008_6610;
    // SAFETY: an ext4 mount descriptor and a pointer to one u64 for the ioctl to read.
    if unsafe { libc::ioctl(mount.as_raw_fd(), RESIZE_FS as _, &target_blocks) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: syncfs flushes the filesystem associated with this open descriptor.
    if unsafe { libc::syncfs(mount.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let after = geometry(device)?.bytes;
    if after != target_bytes {
        return Err(io::Error::other(format!(
            "ext4 reports {after} bytes after growth to {target_bytes}"
        )));
    }
    Ok(Some((before.bytes, after)))
}
