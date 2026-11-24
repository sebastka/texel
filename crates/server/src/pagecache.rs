//! Files read once in bulk (a zipped image indexed, a game hashed): their
//! pages leave the page cache as they are read, rather than filling it.
//! The kernel would take them back when short of memory, but meanwhile
//! they count in the server's memory (a container's limit, a service's
//! cgroup) and push out what is read again.
#![allow(unsafe_code)]

use std::fs::File;
use std::path::Path;

/// Bytes read between two drops.
pub const STEP: u64 = 64 << 20;

/// Drops what the page cache holds of `file` (clean pages: nothing is
/// lost, it is read from the disk again if needed).
pub fn drop(file: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: advice on an open descriptor, touching no memory of ours.
        unsafe {
            libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = file;
}

/// As `drop`, for a file read through another handle.
pub fn drop_path(path: &Path) {
    if let Ok(f) = File::open(path) {
        drop(&f);
    }
}
