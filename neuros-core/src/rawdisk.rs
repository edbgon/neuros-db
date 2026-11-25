//! Raw disk / image access for the Neuros hard disk connected directly
//! through a USB-IDE adapter.
//!
//! The Neuros DSP talks to the disk in 16-bit words with the opposite byte
//! order, so on a PC every byte pair on the disk appears swapped (as with
//! `dd conv=swab`).  The FAT32 filesystem can't be mounted by the OS then,
//! so we un-swap in a block cache and run our own FAT driver on top.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::vfs::FatVfs;
use crate::{Error, Result};

const BLOCK: u64 = 64 * 1024;
const MAX_BLOCKS: usize = 512; // 32 MiB cache

/// Byte order handling for raw access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapMode {
    /// Detect from the boot signature (55 AA vs AA 55).
    Auto,
    /// Force byte swapping (Neuros disk on a USB-IDE adapter).
    Swapped,
    /// Normal byte order (e.g. a plain disk image).
    Normal,
}

fn swab(buf: &mut [u8]) {
    for pair in buf.as_chunks_mut::<2>().0 {
        pair.swap(0, 1);
    }
}

struct Block {
    data: Vec<u8>,
    /// Bytes actually present on the device (short at the very end).
    valid: usize,
    dirty: bool,
    used: u64,
}

/// A disk or image file with optional 16-bit byte swapping and a
/// write-back block cache.  All I/O to the device is 64 KiB aligned, which
/// also satisfies Windows' sector alignment rules for physical drives.
pub struct BlockDev {
    file: File,
    swap: bool,
    writable: bool,
    cache: HashMap<u64, Block>,
    tick: u64,
}

impl BlockDev {
    pub fn open(path: &Path, swap: bool, writable: bool) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(writable).open(path).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                Error::Other(format!(
                    "permission denied opening {} — raw disk access needs root, or membership of the 'disk' group",
                    path.display()
                ))
            } else {
                Error::Io(e)
            }
        })?;
        Ok(Self { file, swap, writable, cache: HashMap::new(), tick: 0 })
    }

    fn flush_block(file: &mut File, swap: bool, idx: u64, b: &mut Block) -> io::Result<()> {
        if !b.dirty {
            return Ok(());
        }
        let mut out = b.data[..b.valid].to_vec();
        if swap {
            swab(&mut out);
        }
        file.seek(SeekFrom::Start(idx * BLOCK))?;
        file.write_all(&out)?;
        b.dirty = false;
        Ok(())
    }

    fn block(&mut self, idx: u64) -> io::Result<&mut Block> {
        self.tick += 1;
        if !self.cache.contains_key(&idx) {
            if self.cache.len() >= MAX_BLOCKS {
                // Evict the least recently used block.
                let (&old, _) = self.cache.iter().min_by_key(|(_, b)| b.used).unwrap();
                let mut b = self.cache.remove(&old).unwrap();
                Self::flush_block(&mut self.file, self.swap, old, &mut b)?;
            }
            let mut data = vec![0u8; BLOCK as usize];
            self.file.seek(SeekFrom::Start(idx * BLOCK))?;
            let mut valid = 0;
            while valid < data.len() {
                match self.file.read(&mut data[valid..]) {
                    Ok(0) => break,
                    Ok(n) => valid += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if self.swap {
                swab(&mut data[..valid & !1]);
            }
            self.cache.insert(idx, Block { data, valid, dirty: false, used: 0 });
        }
        let t = self.tick;
        let b = self.cache.get_mut(&idx).unwrap();
        b.used = t;
        Ok(b)
    }

    pub fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done as u64;
            let (idx, inner) = (pos / BLOCK, (pos % BLOCK) as usize);
            let b = self.block(idx)?;
            if inner >= b.valid {
                break; // end of device
            }
            let n = (b.valid - inner).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&b.data[inner..inner + n]);
            done += n;
        }
        Ok(done)
    }

    pub fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<usize> {
        if !self.writable {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "device opened read-only"));
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done as u64;
            let (idx, inner) = (pos / BLOCK, (pos % BLOCK) as usize);
            let b = self.block(idx)?;
            if inner >= b.valid {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "write past end of device"));
            }
            let n = (b.valid - inner).min(buf.len() - done);
            b.data[inner..inner + n].copy_from_slice(&buf[done..done + n]);
            b.dirty = true;
            done += n;
        }
        Ok(done)
    }

    /// Write all dirty blocks and fsync.
    pub fn sync(&mut self) -> io::Result<()> {
        let mut idxs: Vec<u64> = self.cache.iter().filter(|(_, b)| b.dirty).map(|(&i, _)| i).collect();
        idxs.sort_unstable();
        for i in idxs {
            let b = self.cache.get_mut(&i).unwrap();
            Self::flush_block(&mut self.file, self.swap, i, b)?;
        }
        if self.writable {
            self.file.sync_all()?;
        }
        Ok(())
    }
}

/// A window onto one partition of a `BlockDev`.
pub struct PartitionDev {
    dev: BlockDev,
    start: u64,
    len: u64,
    pos: u64,
}

impl PartitionDev {
    pub fn new(dev: BlockDev, start: u64, len: u64) -> Self {
        Self { dev, start, len, pos: 0 }
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn sync(&mut self) -> io::Result<()> {
        self.dev.sync()
    }
}

impl Read for PartitionDev {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let avail = self.len.saturating_sub(self.pos).min(buf.len() as u64) as usize;
        let n = self.dev.read_at(self.start + self.pos, &mut buf[..avail])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for PartitionDev {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let avail = self.len.saturating_sub(self.pos).min(buf.len() as u64) as usize;
        if avail == 0 && !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write past end of partition"));
        }
        let n = self.dev.write_at(self.start + self.pos, &buf[..avail])?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        // Data stays in the block cache until sync(); fatfs flushes often.
        Ok(())
    }
}

impl Seek for PartitionDev {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

// ---------------------------------------------------------------- probing

/// What we found at the start of a raw disk or image.
#[derive(Debug, Clone)]
pub struct Probe {
    pub path: PathBuf,
    pub swapped: bool,
    pub start: u64,
    pub len: u64,
    /// e.g. "MBR partition 1 (type 0x0C)" or "no partition table".
    pub layout: String,
    pub fat_type: String,
}

impl Probe {
    pub fn summary(&self) -> String {
        format!(
            "{}, {}, {} {}, {:.1} GB",
            self.path.display(),
            if self.swapped { "byte-swapped (Neuros IDE)" } else { "normal byte order" },
            self.layout,
            self.fat_type,
            self.len as f64 / 1e9
        )
    }
}

fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn le32(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64
}

/// If `s` (un-swapped) is a FAT boot sector, return (fat type, total bytes).
fn fat_boot(s: &[u8]) -> Option<(String, u64)> {
    if s[510] != 0x55 || s[511] != 0xAA || !(s[0] == 0xEB || s[0] == 0xE9) {
        return None;
    }
    let bps = le16(s, 11);
    if !matches!(bps, 512 | 1024 | 2048 | 4096) || s[13] == 0 {
        return None;
    }
    let total = match le16(s, 19) {
        0 => le32(s, 32),
        n => n,
    };
    let ty = if &s[82..87] == b"FAT32" {
        "FAT32"
    } else if &s[54..57] == b"FAT" {
        "FAT16"
    } else if le16(s, 22) == 0 {
        "FAT32"
    } else {
        "FAT"
    };
    Some((ty.to_string(), total * bps))
}

fn read_sector(dev: &mut BlockDev, off: u64) -> Result<Vec<u8>> {
    let mut s = vec![0u8; 512];
    if dev.read_at(off, &mut s)? != 512 {
        return Err(Error::Other("device too small".into()));
    }
    Ok(s)
}

/// Look at a disk or image and find its FAT filesystem.
pub fn probe(path: &Path, mode: SwapMode) -> Result<Probe> {
    let mut raw = BlockDev::open(path, false, false)?;
    let s0 = read_sector(&mut raw, 0)?;
    let swapped = match mode {
        SwapMode::Swapped => true,
        SwapMode::Normal => false,
        SwapMode::Auto => match (s0[510], s0[511]) {
            (0x55, 0xAA) => false,
            (0xAA, 0x55) => true,
            _ => {
                return Err(Error::Other(format!(
                    "{}: no boot signature in either byte order — not a FAT disk, or blank",
                    path.display()
                )));
            }
        },
    };
    let mut dev = BlockDev::open(path, swapped, false)?;
    let s0 = read_sector(&mut dev, 0)?;
    if s0[510..512] != [0x55, 0xAA] {
        return Err(Error::Other(format!("{}: no boot signature with the chosen byte order", path.display())));
    }
    let mk = |start, len, layout: String, fat_type| Probe {
        path: path.to_path_buf(),
        swapped,
        start,
        len,
        layout,
        fat_type,
    };

    // Partitionless ("superfloppy") FAT volume?
    if let Some((ty, len)) = fat_boot(&s0) {
        return Ok(mk(0, len, "no partition table".into(), ty));
    }
    // MBR: first FAT partition.
    let mut seen = Vec::new();
    for i in 0..4 {
        let e = &s0[446 + 16 * i..446 + 16 * (i + 1)];
        let ty = e[4];
        if ty == 0 {
            continue;
        }
        seen.push(format!("0x{ty:02X}"));
        if !matches!(ty, 0x01 | 0x04 | 0x06 | 0x0B | 0x0C | 0x0E) {
            continue;
        }
        let start = le32(e, 8) * 512;
        let len = le32(e, 12) * 512;
        let bs = read_sector(&mut dev, start)?;
        if let Some((fty, _)) = fat_boot(&bs) {
            return Ok(mk(start, len, format!("MBR partition {} (type 0x{ty:02X})", i + 1), fty));
        }
    }
    Err(Error::Other(format!(
        "{}: no FAT partition found (partition types: {})",
        path.display(),
        if seen.is_empty() { "none".into() } else { seen.join(", ") }
    )))
}

/// Open the FAT filesystem described by `p`.
pub fn open_fat(p: &Probe, writable: bool) -> Result<FatVfs> {
    let dev = BlockDev::open(&p.path, p.swapped, writable)?;
    let part = PartitionDev::new(dev, p.start, p.len);
    FatVfs::mount(part, p.summary())
}

// ---------------------------------------------------------------- listing

#[derive(Debug, Clone)]
pub struct RawDisk {
    pub path: PathBuf,
    pub description: String,
    /// Some if it looks like a FAT disk we could open.
    pub probe: Option<Probe>,
    pub error: Option<String>,
}

/// Is any partition of this disk currently mounted by the OS?
pub fn is_mounted(dev: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let name = dev.to_string_lossy().to_string();
        if let Ok(m) = std::fs::read_to_string("/proc/mounts") {
            return m.lines().any(|l| l.split_whitespace().next().is_some_and(|d| d.starts_with(&name)));
        }
    }
    let _ = dev;
    false
}

/// Candidate whole disks attached to this machine.
pub fn list_disks() -> Vec<RawDisk> {
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let mut names: Vec<String> = std::fs::read_dir("/sys/block")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("sd") || n.starts_with("hd"))
            .collect();
        names.sort();
        for n in names {
            let sys = PathBuf::from("/sys/block").join(&n);
            let rd = |f: &str| std::fs::read_to_string(sys.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
            let size = rd("size").parse::<u64>().unwrap_or(0) * 512;
            let model = format!("{} {}", rd("device/vendor"), rd("device/model")).trim().to_string();
            out.push((PathBuf::from("/dev").join(&n), format!("{model} ({:.1} GB)", size as f64 / 1e9)));
        }
    }
    #[cfg(windows)]
    for i in 0..16 {
        out.push((PathBuf::from(format!(r"\\.\PhysicalDrive{i}")), format!("Physical drive {i}")));
    }
    #[cfg(target_os = "macos")]
    for i in 0..16 {
        let p = PathBuf::from(format!("/dev/rdisk{i}"));
        if p.exists() {
            out.push((p, format!("disk{i}")));
        }
    }
    out.into_iter()
        .filter_map(|(path, description)| {
            if is_mounted(&path) {
                return None; // in use by the OS: never our target
            }
            let (probe, error) = match probe(&path, SwapMode::Auto) {
                Ok(p) => (Some(p), None),
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => return None,
                Err(e) => (None, Some(e.to_string())),
            };
            Some(RawDisk { path, description, probe, error })
        })
        .collect()
}
