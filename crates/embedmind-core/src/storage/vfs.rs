//! `trait Vfs` — the seam between the pager and the operating system.
//!
//! All file I/O in the storage layer goes through this trait (the SQLite
//! trick, `docs/TESTING.md` §2) so the crash harness can substitute
//! [`crate::storage::sim::SimVfs`] and inject kill points, torn writes and
//! lying fsyncs deterministically. Production uses [`RealVfs`], a thin
//! passthrough to `std::fs` with positional I/O and advisory file locking
//! (`flock` semantics on Unix, `LockFileEx` on Windows — `docs/FORMAT.md` §9).

use std::fs;
use std::io;
use std::path::Path;

/// How to open a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Open an existing file read-write; error if it does not exist.
    MustExist,
    /// Create a new file; error if it already exists.
    CreateNew,
    /// Open read-write, creating the file if needed (WAL sidecar).
    OpenOrCreate,
}

/// An open file handle. Methods take `&self` (positional I/O, like
/// `pread`/`pwrite`); implementations provide their own interior mutability.
pub trait VfsFile: Send {
    /// Reads exactly `buf.len()` bytes at `offset`; short reads are errors
    /// (`ErrorKind::UnexpectedEof`).
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Writes all of `buf` at `offset`, extending the file if needed.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Durably flushes file contents to storage (`fsync`; `FlushFileBuffers`
    /// on Windows).
    fn sync(&self) -> io::Result<()>;

    /// Truncates or extends the file to `len` bytes.
    fn truncate(&self, len: u64) -> io::Result<()>;

    /// Current file length in bytes.
    fn len(&self) -> io::Result<u64>;

    /// Whether the file is empty (zero length).
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Attempts to take an advisory exclusive lock on the whole file.
    /// `Ok(false)` = held by someone else. The lock is released when the
    /// handle is dropped.
    fn try_lock_exclusive(&self) -> io::Result<bool>;
}

/// A file system. Owns file opening/deletion so tests can run entirely
/// in memory.
pub trait Vfs: Send + Sync {
    /// Opens `path` according to `mode`.
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn VfsFile>>;

    /// Deletes `path`. Deleting a missing file is an error.
    fn delete(&self, path: &Path) -> io::Result<()>;

    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;

    /// Atomically renames `from` to `to`, replacing `to` if it exists. This is
    /// the primitive `vacuum` swaps the rebuilt file in with (`docs/adr/0003`):
    /// a crash either leaves the old file fully in place or the new one fully
    /// in place, never a torn mix. `std::fs::rename` gives this on both Unix
    /// (`rename(2)`) and Windows (`MoveFileEx` with replace semantics).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
}

/// Production VFS: thin passthrough to `std::fs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealVfs;

impl Vfs for RealVfs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn VfsFile>> {
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true);
        match mode {
            OpenMode::MustExist => {}
            OpenMode::CreateNew => {
                opts.create_new(true);
            }
            OpenMode::OpenOrCreate => {
                opts.create(true);
            }
        }
        Ok(Box::new(RealFile {
            file: opts.open(path)?,
        }))
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        // `fs::rename` replaces an existing destination atomically on both
        // supported platforms (POSIX `rename`, Windows `MoveFileExW` with
        // `MOVEFILE_REPLACE_EXISTING`).
        fs::rename(from, to)
    }
}

struct RealFile {
    file: fs::File,
}

impl VfsFile for RealFile {
    #[cfg(unix)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)
    }

    #[cfg(windows)]
    fn read_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match self.file.seek_read(buf, offset) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "read past end of file",
                    ));
                }
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(buf, offset)
    }

    #[cfg(windows)]
    fn write_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match self.file.seek_write(buf, offset) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write page",
                    ));
                }
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn truncate(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn try_lock_exclusive(&self) -> io::Result<bool> {
        match self.file.try_lock() {
            Ok(()) => Ok(true),
            Err(fs::TryLockError::WouldBlock) => Ok(false),
            Err(fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::path::PathBuf;

    fn temp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "embedmind-vfs-test-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn real_vfs_roundtrip_and_lock() {
        let vfs = RealVfs;
        let path = temp_path("roundtrip");
        let f = vfs.open(&path, OpenMode::CreateNew).unwrap();
        assert!(f.try_lock_exclusive().unwrap());

        f.write_at(b"hello", 3).unwrap();
        assert_eq!(f.len().unwrap(), 8);
        let mut buf = [0u8; 5];
        f.read_at(&mut buf, 3).unwrap();
        assert_eq!(&buf, b"hello");

        // Second handle must see the exclusive lock.
        let g = vfs.open(&path, OpenMode::MustExist).unwrap();
        assert!(!g.try_lock_exclusive().unwrap());
        drop(g);

        f.truncate(4).unwrap();
        assert_eq!(f.len().unwrap(), 4);
        f.sync().unwrap();
        assert!(f.read_at(&mut buf, 3).is_err());

        drop(f);
        vfs.delete(&path).unwrap();
        assert!(!vfs.exists(&path));
    }

    #[test]
    fn real_vfs_open_modes() {
        let vfs = RealVfs;
        let path = temp_path("modes");
        assert!(vfs.open(&path, OpenMode::MustExist).is_err());
        let f = vfs.open(&path, OpenMode::OpenOrCreate).unwrap();
        assert!(vfs.open(&path, OpenMode::CreateNew).is_err());
        drop(f);
        vfs.delete(&path).unwrap();
    }
}
