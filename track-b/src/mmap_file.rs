//! Zero-copy, read-only memory-mapped file access.
//!
//! std doesn't expose mmap (unlike `read_at`, which IS in std, there
//! is no `std::os::unix::fs::MmapExt`), so this hand-declares exactly
//! the three POSIX calls needed. This is NOT a new dependency: every
//! Rust binary on Linux already links libc (std's own file I/O,
//! allocator and thread spawning all go through it) -- `extern "C"`
//! here just calls into C library functions that are already part of
//! the binary, the same way `std::os::unix::fs::FileExt::read_at`
//! (used elsewhere in this codebase) is std's own thin wrapper over
//! the `pread` syscall.
//!
//! Why mmap instead of `fs::read`: `fs::read` issues a `read(2)` that
//! copies the file's bytes from the kernel's page cache into a freshly
//! allocated heap buffer -- for `docs.txt` (254 MB at `full`), a real
//! memcpy. `mmap` instead maps the page-cache pages directly into
//! this process's address
//! space: no copy, and (since this is `PROT_READ`/`MAP_PRIVATE`, never
//! written to) no copy-on-write fault either. `MADV_WILLNEED` asks the
//! kernel to start reading ahead immediately rather than lazily
//! faulting page by page; `MADV_SEQUENTIAL` additionally hints that
//! access won't jump backward, letting the kernel drop pages behind
//! the read pointer sooner.
//!
//! SAFETY (read before touching this file): `mmap`/`munmap`/`madvise`
//! are inherently unsafe FFI calls straight into the kernel. This
//! module's safety rests on four invariants, all enforced by the code
//! below, not by the caller:
//!   1. `fd` is a valid, open, readable file descriptor for as long as
//!      the mapping exists. POSIX actually allows closing the fd right
//!      after `mmap` returns (the mapping stays valid), but this keeps
//!      the `File` alive inside `MappedFile` anyway -- simpler to
//!      reason about, costs nothing.
//!   2. The `length` passed to `mmap`, every `madvise`, and `munmap` is
//!      always the SAME value: the file's own size, read once at open
//!      time and never recomputed.
//!   3. The returned pointer is only ever exposed as a `&[u8]` (never
//!      `&mut`), of exactly that length, and only for as long as the
//!      `MappedFile` is alive -- enforced by `as_slice`'s `&self`
//!      borrow, since the mapping is `PROT_READ`: nothing can write
//!      through this pointer, so sharing `&[u8]` views of it across
//!      threads has no data race to have.
//!   4. `munmap` runs exactly once, in `Drop`, which Rust guarantees
//!      runs at most once per value.

use std::io;

#[cfg(unix)]
mod ffi {
    use std::ffi::c_void;

    pub const PROT_READ: i32 = 0x1;
    pub const MAP_PRIVATE: i32 = 0x02;
    pub const MAP_FAILED: usize = usize::MAX; // (void *) -1, the POSIX failure sentinel
    pub const MADV_SEQUENTIAL: i32 = 2;
    pub const MADV_WILLNEED: i32 = 3;

    // Exact POSIX signatures (`man 2 mmap`, `man 2 madvise`): libc is
    // already linked into every Rust binary, so this declares calls
    // into code that's already there, not a new dependency.
    extern "C" {
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> i32;
        pub fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    }
}

#[cfg(unix)]
pub struct MappedFile {
    _file: std::fs::File, // kept open for the mapping's whole lifetime; see module doc point 1
    ptr: *mut std::ffi::c_void,
    len: usize,
}

#[cfg(unix)]
impl MappedFile {
    /// Maps `path` read-only. `sequential` additionally hints
    /// `MADV_SEQUENTIAL` for a single forward pass; `MADV_WILLNEED` is
    /// always set.
    pub fn open(path: &str, sequential: bool) -> io::Result<Self> {
        use std::os::fd::AsRawFd;

        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            // mmap() of zero length is undefined by POSIX; an empty
            // file has no bytes to map, so just hand back an empty slice.
            return Ok(MappedFile {
                _file: file,
                ptr: std::ptr::null_mut(),
                len: 0,
            });
        }
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is valid and open (just obtained above); `len`
        // is the file's own size; PROT_READ+MAP_PRIVATE never writes
        // through the mapping, so there is no aliasing/COW hazard.
        let ptr = unsafe {
            ffi::mmap(
                std::ptr::null_mut(),
                len,
                ffi::PROT_READ,
                ffi::MAP_PRIVATE,
                fd,
                0,
            )
        };
        if ptr as usize == ffi::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `ptr`/`len` exactly match the successful mmap() above.
        // madvise() failing is not fatal -- it's a performance hint,
        // never required for correctness -- so its return value is
        // deliberately ignored.
        unsafe {
            if sequential {
                ffi::madvise(ptr, len, ffi::MADV_SEQUENTIAL);
            }
            ffi::madvise(ptr, len, ffi::MADV_WILLNEED);
        }
        Ok(MappedFile {
            _file: file,
            ptr,
            len,
        })
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `ptr` is a live mapping of exactly `self.len`
        // readable bytes for as long as `self` exists (munmap only
        // runs in Drop, below, which cannot run while this borrow is
        // outstanding). The mapping is PROT_READ-only, so no writer
        // can ever alias this `&[u8]`.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

// SAFETY: the mapping is PROT_READ for its entire lifetime, so
// sharing `&MappedFile` across threads only ever allows concurrent
// reads of the same immutable bytes -- never a data race.
#[cfg(unix)]
unsafe impl Sync for MappedFile {}
#[cfg(unix)]
unsafe impl Send for MappedFile {}

#[cfg(unix)]
impl Drop for MappedFile {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: matches the mmap() call in `open` exactly (same
            // pointer, same length); Drop runs at most once per value,
            // so this munmap runs at most once.
            unsafe { ffi::munmap(self.ptr, self.len) };
        }
    }
}

// Windows fallback, local dev/testing only -- the scored target is
// always Linux. Not zero-copy (reads the whole file into an owned
// buffer), but behaves identically from the caller's point of view,
// so the rest of the track doesn't need a second code path.
#[cfg(windows)]
pub struct MappedFile {
    data: Vec<u8>,
}

#[cfg(windows)]
impl MappedFile {
    pub fn open(path: &str, _sequential: bool) -> io::Result<Self> {
        Ok(MappedFile {
            data: std::fs::read(path)?,
        })
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn with_temp_file<R>(data: &[u8], f: impl FnOnce(&str) -> R) -> R {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mmap_file_test_{}_{}.bin",
            std::process::id(),
            unique
        ));
        {
            let mut file = File::create(&path).expect("create temp file");
            file.write_all(data).expect("write temp file");
        }
        let result = f(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn maps_a_small_file_and_reads_back_the_exact_bytes() {
        let data = b"hello, mmap world\n";
        with_temp_file(data, |path| {
            let mapped = MappedFile::open(path, true).expect("mmap open");
            assert_eq!(mapped.as_slice(), data);
        });
    }

    #[test]
    fn maps_an_empty_file_as_an_empty_slice() {
        with_temp_file(b"", |path| {
            let mapped = MappedFile::open(path, false).expect("mmap open");
            assert_eq!(mapped.as_slice(), b"");
        });
    }

    #[test]
    fn maps_a_multi_page_file_correctly() {
        // Larger than one 4 KiB page, to exercise a real multi-page mapping.
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 256) as u8).collect();
        with_temp_file(&data, |path| {
            let mapped = MappedFile::open(path, true).expect("mmap open");
            assert_eq!(mapped.as_slice(), data.as_slice());
        });
    }
}
