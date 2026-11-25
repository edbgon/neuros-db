//! End-to-end: a byte-swapped disk image (like the Neuros hard disk on a
//! USB-IDE adapter) is detected, scanned and written through our FAT
//! driver, and comes out as a valid filesystem with a valid database.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use neuros_core::rawdisk::{self, BlockDev, PartitionDev, SwapMode};
use neuros_core::reader::load_database;
use neuros_core::vfs::{self, FatVfs, Vfs};
use neuros_core::{BuildOptions, Drive, ScanOptions, Source, WriteOptions};

const SECTOR: u64 = 512;
const PART_START: u64 = 2048;
const PART_SECTORS: u64 = 160 * 1024 * 2; // 160 MiB

/// A few silent MPEG-1 Layer III frames (128 kbit/s, 44.1 kHz) with an ID3v1 tag.
fn fake_mp3(title: &str, artist: &str, album: &str, track: u8, frames: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..frames {
        v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        v.extend(std::iter::repeat_n(0u8, 417 - 4));
    }
    let mut tag = vec![0u8; 128];
    tag[..3].copy_from_slice(b"TAG");
    let put = |t: &mut Vec<u8>, off: usize, s: &str| {
        t[off..off + s.len().min(30)].copy_from_slice(&s.as_bytes()[..s.len().min(30)])
    };
    put(&mut tag, 3, title);
    put(&mut tag, 33, artist);
    put(&mut tag, 63, album);
    tag[126] = track; // ID3v1.1 track number
    tag[127] = 13; // Pop
    v.extend(tag);
    v
}

fn swab_file(path: &Path) {
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    loop {
        f.seek(SeekFrom::Start(off)).unwrap();
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        for p in buf[..n].as_chunks_mut::<2>().0 {
            p.swap(0, 1);
        }
        f.seek(SeekFrom::Start(off)).unwrap();
        f.write_all(&buf[..n]).unwrap();
        off += n as u64;
    }
}

fn make_image(path: &Path) -> Vec<(String, Vec<u8>)> {
    let total = (PART_START + PART_SECTORS) * SECTOR;
    let f = std::fs::File::create(path).unwrap();
    f.set_len(total).unwrap();
    drop(f);

    // MBR with one FAT32 LBA partition.
    let mut mbr = vec![0u8; 512];
    let e = &mut mbr[446..462];
    e[4] = 0x0C;
    e[8..12].copy_from_slice(&(PART_START as u32).to_le_bytes());
    e[12..16].copy_from_slice(&(PART_SECTORS as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.write_all(&mbr).unwrap();
    drop(f);

    let dev = BlockDev::open(path, false, true).unwrap();
    let mut part = PartitionDev::new(dev, PART_START * SECTOR, PART_SECTORS * SECTOR);
    fatfs::format_volume(&mut part, fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32)).unwrap();
    part.seek(SeekFrom::Start(0)).unwrap();
    let fs = Box::new(FatVfs::mount(part, "test".into()).unwrap());

    let files = vec![
        ("MUSIC/ABBA/Gold/01 Dancing Queen.mp3".to_string(), fake_mp3("Dancing Queen", "ABBA", "Gold", 1, 300)),
        ("MUSIC/ABBA/Gold/02 SOS.mp3".to_string(), fake_mp3("SOS", "ABBA", "Gold", 2, 200)),
        ("MUSIC/Björk/Jóga.mp3".to_string(), fake_mp3("Joga", "Bjork", "Homogenic", 3, 150)),
    ];
    fs.write_file("sn.txt", b"7654321\r\n").unwrap();
    for (p, data) in &files {
        vfs::create_dir_all(fs.as_ref(), vfs::parent(p)).unwrap();
        fs.write_file(p, data).unwrap();
    }
    fs.finish().unwrap();
    files
}

#[test]
fn byte_swapped_disk_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("neuros.img");
    let files = make_image(&img);
    swab_file(&img);

    // Raw bytes really are swapped now.
    let mut s0 = [0u8; 512];
    std::fs::File::open(&img).unwrap().read_exact(&mut s0).unwrap();
    assert_eq!(&s0[510..], &[0xAA, 0x55]);

    let p = rawdisk::probe(&img, SwapMode::Auto).unwrap();
    assert!(p.swapped);
    assert_eq!(p.start, PART_START * SECTOR);
    assert_eq!(p.fat_type, "FAT32");

    // Scan + write through the swapped driver.
    let src = Source::Raw { path: img.clone(), swap: SwapMode::Auto };
    let (fs, probe) = src.open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), probe);
    assert_eq!(info.serial.as_deref(), Some("7654321"));
    assert_eq!(info.drive, Drive::C);
    let opts = ScanOptions { music_dir: "music".into(), include_recordings: true, placeholders: true };
    let items =
        neuros_core::scan(fs.as_ref(), info.drive, &opts, &Default::default(), &AtomicBool::new(false), |_, _| {})
            .unwrap();
    assert_eq!(items.len(), 3);
    let dq = items.iter().find(|i| i.track.title == "Dancing Queen").unwrap();
    assert_eq!(dq.track.artist.as_deref(), Some("ABBA"));
    assert_eq!(dq.track.duration_secs, 8); // 300 frames * 1152 / 44100 ≈ 7.8 s
    let wopts = WriteOptions {
        build: BuildOptions { artist_albums: true, ..Default::default() },
        rename_files: true,
        backup_dir: None,
        drive: None,
        dry_run: false,
    };
    let rep = neuros_core::write_database(fs.as_ref(), &info, &items, &wopts).unwrap();
    assert_eq!(rep.tracks_written, 3);
    assert_eq!(rep.renamed.len(), 1);
    fs.finish().unwrap();

    // Still swapped on disk.
    std::fs::File::open(&img).unwrap().read_exact(&mut s0).unwrap();
    assert_eq!(&s0[510..], &[0xAA, 0x55]);

    // Un-swap and read it back in normal byte order.
    swab_file(&img);
    let src = Source::Raw { path: img.clone(), swap: SwapMode::Normal };
    let (fs, _) = src.open(false).unwrap();
    let db = load_database(fs.as_ref(), "WOID_DB/audio", "audio").unwrap();
    assert_eq!(neuros_core::verify::verify(&db), Vec::<String>::new());
    let aa = db.children.iter().find(|c| c.file_stem == "albums").unwrap();
    assert_eq!(aa.subs[0].names(), ["ABBA", "Bjork"]);
    let mut paths: Vec<_> = db.tracks().into_iter().map(|t| t.path).collect();
    paths.sort();
    assert_eq!(
        paths,
        ["C:/MUSIC/ABBA/Gold/01 Dancing Queen.mp3", "C:/MUSIC/ABBA/Gold/02 SOS.mp3", "C:/MUSIC/Bjork/Joga.mp3"]
    );
    // Music data survived untouched (renamed file too).
    for (p, data) in &files {
        let p = if p.contains("Björk") { "MUSIC/Bjork/Joga.mp3" } else { p.as_str() };
        assert_eq!(&fs.read_file(p).unwrap(), data, "{p}");
    }
    assert!(vfs::find_dir_ci(fs.as_ref(), "MUSIC", "Björk").is_none(), "old dir removed");
    for db in ["pcaudio", "idedhisi", "unidedhisi", "failedhisi"] {
        assert!(vfs::find_dir_ci(fs.as_ref(), "WOID_DB", db).is_some());
    }

    // Keep the image for an external fsck when requested.
    if let Some(keep) = std::env::var_os("NEUROS_KEEP_IMAGE") {
        std::fs::copy(&img, keep).unwrap();
    }
}

#[test]
fn read_only_open_refuses_writes() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("ro.img");
    make_image(&img);
    let src = Source::Raw { path: img, swap: SwapMode::Normal };
    let (fs, _) = src.open(false).unwrap();
    assert!(fs.write_file("x.txt", b"no").is_err());
}
