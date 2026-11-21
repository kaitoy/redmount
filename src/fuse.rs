//! FUSE adapter (Linux: fusermount, macOS: macFUSE): maps `fuse_mt` calls onto the `fs` core.
use crate::fs::{E, EPOCH, RedmineFs, Stat};
use fuse_mt::{
    CallbackResult, CreatedEntry, DirectoryEntry, FileAttr, FileType, FilesystemMT, FuseMT, RequestInfo, ResultCreate,
    ResultEmpty, ResultEntry, ResultOpen, ResultReaddir, ResultSlice, ResultStatfs, ResultWrite, Statfs,
};
use std::ffi::{OsStr, c_int};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);
/// Bypass the page cache: rendered files change on the server (FOPEN_DIRECT_IO).
const DIRECT_IO: u32 = 1;

impl From<E> for c_int {
    fn from(e: E) -> Self {
        match e {
            E::NotFound | E::PathNotFound => libc::ENOENT,
            E::Denied => libc::EACCES,
            E::Exists => libc::EEXIST,
            E::NotDir => libc::ENOTDIR,
            E::NotEmpty => libc::ENOTEMPTY,
            E::Io => libc::EIO,
        }
    }
}

/// `/a/b` -> `\a\b`. ponytail: a `\` inside a name acts as a separator.
fn p(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

fn join(parent: &Path, name: &OsStr) -> String {
    p(&parent.join(name))
}

fn time(t: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(t.saturating_sub(EPOCH) * 100)
}

fn attr(req: &RequestInfo, s: Stat) -> FileAttr {
    FileAttr {
        size: s.size,
        blocks: s.size.div_ceil(512),
        atime: time(s.modified),
        mtime: time(s.modified),
        ctime: time(s.modified),
        crtime: time(s.created),
        kind: if s.dir { FileType::Directory } else { FileType::RegularFile },
        perm: if s.dir { 0o755 } else { 0o644 },
        nlink: if s.dir { 2 } else { 1 },
        uid: req.uid,
        gid: req.gid,
        rdev: 0,
        flags: 0,
    }
}

struct Fuse(RedmineFs);

impl FilesystemMT for Fuse {
    fn getattr(&self, req: RequestInfo, path: &Path, _fh: Option<u64>) -> ResultEntry {
        Ok((TTL, attr(&req, self.0.stat(&p(path))?)))
    }

    fn chmod(&self, _req: RequestInfo, _path: &Path, _fh: Option<u64>, _mode: u32) -> ResultEmpty {
        Ok(())
    }

    fn chown(&self, _req: RequestInfo, _path: &Path, _fh: Option<u64>, _uid: Option<u32>, _gid: Option<u32>) -> ResultEmpty {
        Ok(())
    }

    fn truncate(&self, _req: RequestInfo, path: &Path, _fh: Option<u64>, size: u64) -> ResultEmpty {
        self.0.set_size(&p(path), size, false)?;
        Ok(())
    }

    fn utimens(&self, _req: RequestInfo, _path: &Path, _fh: Option<u64>, _a: Option<SystemTime>, _m: Option<SystemTime>) -> ResultEmpty {
        Ok(())
    }

    fn mkdir(&self, req: RequestInfo, parent: &Path, name: &OsStr, _mode: u32) -> ResultEntry {
        let path = join(parent, name);
        let s = self.0.create(&path, true)?;
        self.0.close(&path); // no handle stays open after mkdir
        Ok((TTL, attr(&req, s)))
    }

    fn unlink(&self, _req: RequestInfo, parent: &Path, name: &OsStr) -> ResultEmpty {
        let path = join(parent, name);
        self.0.check_delete(&path)?;
        self.0.cleanup(&path, true);
        Ok(())
    }

    fn rmdir(&self, req: RequestInfo, parent: &Path, name: &OsStr) -> ResultEmpty {
        self.unlink(req, parent, name)
    }

    fn rename(&self, _req: RequestInfo, parent: &Path, name: &OsStr, newparent: &Path, newname: &OsStr) -> ResultEmpty {
        // POSIX rename always replaces the target
        self.0.rename(&join(parent, name), &join(newparent, newname), true)?;
        Ok(())
    }

    fn open(&self, _req: RequestInfo, path: &Path, _flags: u32) -> ResultOpen {
        self.0.open(&p(path))?;
        Ok((0, DIRECT_IO))
    }

    fn read(&self, _req: RequestInfo, path: &Path, _fh: u64, offset: u64, size: u32, callback: impl FnOnce(ResultSlice<'_>) -> CallbackResult) -> CallbackResult {
        let mut buf = vec![0; size as usize];
        match self.0.read(&p(path), &mut buf, offset) {
            Ok(n) => callback(Ok(&buf[..n])),
            Err(e) => callback(Err(e.into())),
        }
    }

    fn write(&self, _req: RequestInfo, path: &Path, _fh: u64, offset: u64, data: Vec<u8>, _flags: u32) -> ResultWrite {
        Ok(self.0.write(&p(path), &data, offset, false, false)?.0 as u32)
    }

    fn flush(&self, _req: RequestInfo, _path: &Path, _fh: u64, _lock_owner: u64) -> ResultEmpty {
        Ok(())
    }

    fn release(&self, _req: RequestInfo, path: &Path, _fh: u64, _flags: u32, _lock_owner: u64, _flush: bool) -> ResultEmpty {
        let path = p(path);
        self.0.cleanup(&path, false);
        self.0.close(&path);
        Ok(())
    }

    fn fsync(&self, _req: RequestInfo, _path: &Path, _fh: u64, _datasync: bool) -> ResultEmpty {
        Ok(())
    }

    fn opendir(&self, _req: RequestInfo, _path: &Path, _flags: u32) -> ResultOpen {
        Ok((0, 0))
    }

    fn readdir(&self, _req: RequestInfo, path: &Path, _fh: u64) -> ResultReaddir {
        let dot = |n: &str| DirectoryEntry { name: n.into(), kind: FileType::Directory };
        let mut out = vec![dot("."), dot("..")];
        for (name, s) in self.0.read_dir(&p(path))? {
            out.push(DirectoryEntry {
                name: name.into(),
                kind: if s.dir { FileType::Directory } else { FileType::RegularFile },
            });
        }
        Ok(out)
    }

    fn releasedir(&self, _req: RequestInfo, _path: &Path, _fh: u64, _flags: u32) -> ResultEmpty {
        Ok(())
    }

    fn statfs(&self, _req: RequestInfo, _path: &Path) -> ResultStatfs {
        Ok(Statfs { blocks: 1 << 28, bfree: 1 << 27, bavail: 1 << 27, files: 0, ffree: 0, bsize: 4096, namelen: 255, frsize: 4096 })
    }

    fn create(&self, req: RequestInfo, parent: &Path, name: &OsStr, _mode: u32, _flags: u32) -> ResultCreate {
        let s = self.0.create(&join(parent, name), false)?;
        Ok(CreatedEntry { ttl: TTL, attr: attr(&req, s), fh: 0, flags: DIRECT_IO })
    }
}

pub fn run(fs: RedmineFs, mount: &str) -> anyhow::Result<()> {
    let mut opts = vec![OsStr::new("fsname=redmount")];
    if cfg!(target_os = "macos") {
        // noappledouble: keeps Finder's `._issue.md` from being posted as a comment
        opts.extend([OsStr::new("noappledouble"), OsStr::new("volname=Redmine")]);
    }
    let session = fuse_mt::spawn_mount(FuseMT::new(Fuse(fs), 1), mount, &opts)?;
    println!("Mounted on {mount}. Press Enter to unmount.");
    crate::wait()?;
    drop(session);
    Ok(())
}
