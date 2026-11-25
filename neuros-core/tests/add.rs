use std::path::Path;
use std::sync::atomic::AtomicBool;

use neuros_core::reader::load_database;
use neuros_core::vfs::{StdVfs, Vfs};
use neuros_core::{AddLayout, BuildOptions, ScanOptions, Source, WriteOptions};

fn fake_mp3(title: &str, artist: &str, album: &str, track: u8, frames: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..frames {
        v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        v.extend(std::iter::repeat_n(0u8, 417 - 4));
    }
    let mut tag = vec![0u8; 128];
    tag[..3].copy_from_slice(b"TAG");
    for (off, s) in [(3, title), (33, artist), (63, album)] {
        tag[off..off + s.len()].copy_from_slice(s.as_bytes());
    }
    tag[126] = track;
    v.extend(tag);
    v
}

fn put(p: &Path, data: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

fn opts() -> WriteOptions {
    WriteOptions {
        build: BuildOptions { artist_albums: true, ..Default::default() },
        rename_files: true,
        backup_dir: None,
        drive: None,
        dry_run: false,
    }
}

fn scan_opts() -> ScanOptions {
    ScanOptions { music_dir: String::new(), include_recordings: true, placeholders: true }
}

#[test]
fn add_files_updates_database_without_rescan() {
    let dev = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let root = dev.path();
    std::fs::write(root.join("sn.txt"), "1").unwrap();
    // Existing library: album with tracks 2,1 on disk; build its database.
    put(&root.join("MUSIC/ABBA/Gold/b.mp3"), &fake_mp3("SOS", "ABBA", "Gold", 2, 50));
    put(&root.join("MUSIC/ABBA/Gold/a.mp3"), &fake_mp3("Dancing Queen", "ABBA", "Gold", 1, 50));
    let src = Source::Mounted(root.to_path_buf());
    let (fs, p) = src.open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), p);
    let items = neuros_core::scan(
        fs.as_ref(),
        info.drive,
        &scan_opts(),
        &Default::default(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    neuros_core::write_database(fs.as_ref(), &info, &items, &opts()).unwrap();

    // New files on the host, one with a non-ASCII name, one already on the device.
    put(&host.path().join("in/Jóga.mp3"), &fake_mp3("Joga", "Bjork", "Homogenic", 1, 40));
    put(&host.path().join("in/Hunter.mp3"), &fake_mp3("Hunter", "Bjork", "Homogenic", 2, 40));
    put(&host.path().join("in/a.mp3"), &fake_mp3("Dancing Queen", "ABBA", "Gold", 1, 50));
    std::fs::write(host.path().join("in/notes.txt"), "x").unwrap();

    let files = neuros_core::device::collect_audio_files(&host.path().join("in"));
    assert_eq!(files.len(), 3);
    let plan = neuros_core::plan_add(&files, &AddLayout::ByTags, "MUSIC", true);
    let mut dests: Vec<_> = plan.iter().map(|p| p.dest.as_str()).collect();
    dests.sort();
    assert_eq!(dests, ["MUSIC/ABBA/Gold/a.mp3", "MUSIC/Bjork/Homogenic/Hunter.mp3", "MUSIC/Bjork/Homogenic/Joga.mp3"]);

    let info = neuros_core::inspect(fs.as_ref(), None);
    let mut last = (0, 0);
    let rep = neuros_core::add_files(fs.as_ref(), &info, &plan, &opts(), &scan_opts(), |d, t| last = (d, t)).unwrap();
    fs.finish().unwrap();
    assert_eq!(rep.added.len(), 2);
    assert_eq!(rep.already_present, ["MUSIC/ABBA/Gold/a.mp3"]);
    assert_eq!(last.0, last.1);
    assert!(root.join("MUSIC/Bjork/Homogenic/Joga.mp3").exists());

    let vfs = StdVfs::new(root.to_path_buf());
    let db = load_database(&vfs, "WOID_DB/audio", "audio").unwrap();
    assert_eq!(neuros_core::verify::verify(&db), Vec::<String>::new());
    let tracks = db.tracks();
    assert_eq!(tracks.len(), 4, "no duplicate for the file that was already there");
    // Album order kept from the old database, new album ordered by tag.
    let albums = db.children.iter().find(|c| c.file_stem == "albums").unwrap();
    let order = |name: &str| -> Vec<String> {
        let e = albums.sai.iter().find(|e| albums.mdb.records[&e.mdb].primary().as_deref() == Some(name)).unwrap();
        albums.pai[&e.pai].entries.iter().map(|p| db.mdb.records[p].primary().unwrap()).collect()
    };
    assert_eq!(order("Gold"), ["Dancing Queen", "SOS"]);
    assert_eq!(order("Homogenic"), ["Joga", "Hunter"]);
    // Artist → Album menu includes the new artist.
    assert_eq!(albums.subs[0].names(), ["ABBA", "Bjork"]);
    let _ = vfs.describe();
}

#[test]
fn add_files_without_database_scans_device() {
    let dev = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let root = dev.path();
    std::fs::write(root.join("sn.txt"), "1").unwrap();
    put(&root.join("MUSIC/old.mp3"), &fake_mp3("Old", "X", "Y", 1, 20));
    put(&host.path().join("new.mp3"), &fake_mp3("New", "X", "Y", 2, 20));
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    assert!(info.existing.is_none());
    let plan = neuros_core::plan_add(&[host.path().join("new.mp3")], &AddLayout::Folder("MUSIC".into()), "MUSIC", true);
    assert_eq!(plan[0].dest, "MUSIC/new.mp3");
    neuros_core::add_files(fs.as_ref(), &info, &plan, &opts(), &scan_opts(), |_, _| {}).unwrap();
    let db = load_database(&StdVfs::new(root.to_path_buf()), "WOID_DB/audio", "audio").unwrap();
    let mut titles: Vec<_> = db.tracks().into_iter().map(|t| t.title).collect();
    titles.sort();
    assert_eq!(titles, ["New", "Old"]);
}
