//! Minimal filesystem abstraction so the same scan/write code works on a
//! mounted device (std::fs) and on a raw, possibly byte-swapped, disk
//! (our own FAT driver via `fatfs`).
//!
//! Paths are device-relative, '/'-separated, without a leading slash;
//! "" is the device root.

use std::cell::RefCell;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::rawdisk::PartitionDev;
use crate::{Error, Result};

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub len: u64,
}

pub trait Vfs {
    /// Human-readable description of where this is.
    fn describe(&self) -> String;
    fn read_dir(&self, rel: &str) -> Result<Vec<DirEntry>>;
    fn open_read(&self, rel: &str) -> Result<Box<dyn ReadSeek + '_>>;
    fn write_file(&self, rel: &str, data: &[u8]) -> Result<()>;
    fn create_dir(&self, rel: &str) -> Result<()>;
    /// Remove a file or an empty directory.
    fn remove(&self, rel: &str) -> Result<()>;
    fn rename(&self, from: &str, to: &str) -> Result<()>;
    fn total_space(&self) -> Option<u64>;
    /// Flush everything to the medium.  Must be called after writing.
    fn finish(self: Box<Self>) -> Result<()>;

    fn read_file(&self, rel: &str) -> Result<Vec<u8>> {
        let mut v = Vec::new();
        self.open_read(rel)?.read_to_end(&mut v)?;
        Ok(v)
    }
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

pub fn parent(rel: &str) -> &str {
    rel.rfind('/').map(|i| &rel[..i]).unwrap_or("")
}

pub fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Find an entry case-insensitively; returns its real relative path.
pub fn find_ci(vfs: &dyn Vfs, dir: &str, name: &str) -> Option<(String, DirEntry)> {
    vfs.read_dir(dir).ok()?.into_iter().find(|e| e.name.eq_ignore_ascii_case(name)).map(|e| (join(dir, &e.name), e))
}

pub fn find_dir_ci(vfs: &dyn Vfs, dir: &str, name: &str) -> Option<String> {
    find_ci(vfs, dir, name).filter(|(_, e)| e.is_dir).map(|(p, _)| p)
}

pub fn find_file_ci(vfs: &dyn Vfs, dir: &str, name: &str) -> Option<String> {
    find_ci(vfs, dir, name).filter(|(_, e)| !e.is_dir).map(|(p, _)| p)
}

/// Resolve a whole relative path to a file case-insensitively, folder by
/// folder; returns the path with the case actually on disk.
pub fn resolve_file_ci(vfs: &dyn Vfs, rel: &str) -> Option<String> {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let (name, dirs) = parts.split_last()?;
    let mut cur = String::new();
    for d in dirs {
        cur = find_dir_ci(vfs, &cur, d)?;
    }
    find_file_ci(vfs, &cur, name)
}

pub fn exists(vfs: &dyn Vfs, rel: &str) -> bool {
    rel.is_empty() || find_ci(vfs, parent(rel), file_name(rel)).is_some()
}

/// Create `rel` and any missing parents.  Existing components are matched
/// case-insensitively; returns the path with the case actually on disk.
pub fn create_dir_all(vfs: &dyn Vfs, rel: &str) -> Result<String> {
    let mut cur = String::new();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        match find_ci(vfs, &cur, part) {
            Some((p, e)) if e.is_dir => cur = p,
            Some(_) => return Err(Error::Other(format!("{} exists and is not a directory", join(&cur, part)))),
            None => {
                let p = join(&cur, part);
                vfs.create_dir(&p)?;
                cur = p;
            }
        }
    }
    Ok(cur)
}

pub fn remove_dir_all(vfs: &dyn Vfs, rel: &str) -> Result<()> {
    for e in vfs.read_dir(rel)? {
        let p = join(rel, &e.name);
        if e.is_dir {
            remove_dir_all(vfs, &p)?;
        } else {
            vfs.remove(&p)?;
        }
    }
    vfs.remove(rel)
}

/// Copy a directory tree from the device to the host.
pub fn copy_to_host(vfs: &dyn Vfs, rel: &str, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in vfs.read_dir(rel)? {
        let p = join(rel, &e.name);
        if e.is_dir {
            copy_to_host(vfs, &p, &dst.join(&e.name))?;
        } else {
            std::fs::write(dst.join(&e.name), vfs.read_file(&p)?)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- std::fs

/// A mounted device (or any folder).
pub struct StdVfs {
    root: PathBuf,
}

impl StdVfs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn p(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for c in rel.split('/').filter(|c| !c.is_empty()) {
            p.push(c);
        }
        p
    }
}

impl Vfs for StdVfs {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }
    fn read_dir(&self, rel: &str) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(self.p(rel))? {
            let e = e?;
            let ft = e.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            out.push(DirEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir: ft.is_dir(),
                len: e.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
        Ok(out)
    }
    fn open_read(&self, rel: &str) -> Result<Box<dyn ReadSeek + '_>> {
        Ok(Box::new(io::BufReader::new(std::fs::File::open(self.p(rel))?)))
    }
    fn write_file(&self, rel: &str, data: &[u8]) -> Result<()> {
        let mut f = std::fs::File::create(self.p(rel))?;
        f.write_all(data)?;
        f.sync_all()?;
        Ok(())
    }
    fn create_dir(&self, rel: &str) -> Result<()> {
        Ok(std::fs::create_dir(self.p(rel))?)
    }
    fn remove(&self, rel: &str) -> Result<()> {
        let p = self.p(rel);
        if p.is_dir() {
            std::fs::remove_dir(p)?
        } else {
            std::fs::remove_file(p)?
        }
        Ok(())
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        Ok(std::fs::rename(self.p(from), self.p(to))?)
    }
    fn total_space(&self) -> Option<u64> {
        fs4::total_space(&self.root).ok()
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- FAT

/// Storage handle shared between the FAT driver and us, so we can flush
/// the block cache after the filesystem is unmounted.
#[derive(Clone)]
pub struct SharedDev(Rc<RefCell<PartitionDev>>, u64);

impl Read for SharedDev {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut d = self.0.borrow_mut();
        d.seek(io::SeekFrom::Start(self.1))?;
        let n = d.read(buf)?;
        self.1 += n as u64;
        Ok(n)
    }
}
impl Write for SharedDev {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut d = self.0.borrow_mut();
        d.seek(io::SeekFrom::Start(self.1))?;
        let n = d.write(buf)?;
        self.1 += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().flush()
    }
}
impl Seek for SharedDev {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let len = self.0.borrow().len();
        let new = match pos {
            io::SeekFrom::Start(p) => p as i128,
            io::SeekFrom::Current(d) => self.1 as i128 + d as i128,
            io::SeekFrom::End(d) => len as i128 + d as i128,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.1 = new as u64;
        Ok(self.1)
    }
}

type Fs = fatfs::FileSystem<SharedDev>;

/// A FAT filesystem on a raw disk or image, accessed through our driver.
pub struct FatVfs {
    fs: Option<Fs>,
    dev: Rc<RefCell<PartitionDev>>,
    desc: String,
}

fn fat_err(e: io::Error, what: &str, rel: &str) -> Error {
    if e.kind() == io::ErrorKind::NotFound {
        Error::Missing(PathBuf::from(rel))
    } else {
        Error::Other(format!("{what} {rel}: {e}"))
    }
}

impl FatVfs {
    pub fn mount(dev: PartitionDev, desc: String) -> Result<Self> {
        let dev = Rc::new(RefCell::new(dev));
        let opts = fatfs::FsOptions::new().update_accessed_date(false);
        let fs = fatfs::FileSystem::new(SharedDev(dev.clone(), 0), opts)
            .map_err(|e| Error::Other(format!("not a readable FAT filesystem: {e}")))?;
        Ok(Self { fs: Some(fs), dev, desc })
    }

    fn fs(&self) -> &Fs {
        self.fs.as_ref().expect("filesystem already unmounted")
    }

    fn dir(&self, rel: &str) -> Result<fatfs::Dir<'_, SharedDev>> {
        let root = self.fs().root_dir();
        if rel.is_empty() { Ok(root) } else { root.open_dir(rel).map_err(|e| fat_err(e, "open dir", rel)) }
    }

    pub fn block_dev(&self) -> std::cell::Ref<'_, PartitionDev> {
        self.dev.borrow()
    }
}

impl Vfs for FatVfs {
    fn describe(&self) -> String {
        self.desc.clone()
    }
    fn read_dir(&self, rel: &str) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for e in self.dir(rel)?.iter() {
            let e = e.map_err(|e| fat_err(e, "read dir", rel))?;
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            out.push(DirEntry { name, is_dir: e.is_dir(), len: e.len() });
        }
        Ok(out)
    }
    fn open_read(&self, rel: &str) -> Result<Box<dyn ReadSeek + '_>> {
        let f = self.fs().root_dir().open_file(rel).map_err(|e| fat_err(e, "open", rel))?;
        Ok(Box::new(io::BufReader::with_capacity(64 * 1024, f)))
    }
    fn write_file(&self, rel: &str, data: &[u8]) -> Result<()> {
        let mut f = self.fs().root_dir().create_file(rel).map_err(|e| fat_err(e, "create", rel))?;
        f.truncate()?;
        f.write_all(data)?;
        f.flush()?;
        Ok(())
    }
    fn create_dir(&self, rel: &str) -> Result<()> {
        self.fs().root_dir().create_dir(rel).map_err(|e| fat_err(e, "mkdir", rel))?;
        Ok(())
    }
    fn remove(&self, rel: &str) -> Result<()> {
        self.fs().root_dir().remove(rel).map_err(|e| fat_err(e, "remove", rel))
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let root = self.fs().root_dir();
        root.rename(from, &root, to).map_err(|e| fat_err(e, "rename", from))
    }
    fn total_space(&self) -> Option<u64> {
        Some(self.dev.borrow().len())
    }
    fn finish(mut self: Box<Self>) -> Result<()> {
        if let Some(fs) = self.fs.take() {
            fs.unmount()?;
        }
        self.dev.borrow_mut().sync()?;
        Ok(())
    }
}

impl Drop for FatVfs {
    fn drop(&mut self) {
        if let Some(fs) = self.fs.take() {
            let _ = fs.unmount();
        }
        let _ = self.dev.borrow_mut().sync();
    }
}
