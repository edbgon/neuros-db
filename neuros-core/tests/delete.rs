use std::sync::atomic::AtomicBool;

use neuros_core::reader::load_database;
use neuros_core::vfs::StdVfs;
use neuros_core::{BuildOptions, ScanOptions, Source, WriteOptions};

fn mp3(title: &str, artist: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..20 {
        v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        v.extend(std::iter::repeat_n(0u8, 413));
    }
    let mut t = vec![0u8; 128];
    t[..3].copy_from_slice(b"TAG");
    t[3..3 + title.len()].copy_from_slice(title.as_bytes());
    t[33..33 + artist.len()].copy_from_slice(artist.as_bytes());
    v.extend(t);
    v
}

fn put(root: &std::path::Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

#[test]
fn deletes_tracks_and_tolerates_missing_files() {
    let dev = tempfile::tempdir().unwrap();
    let root = dev.path();
    std::fs::write(root.join("sn.txt"), "1").unwrap();
    put(root, "MUSIC/Some Band/First/a.mp3", &mp3("Alpha", "Some Band"));
    put(root, "MUSIC/Some Band/First/b.mp3", &mp3("Bravo", "Some Band"));
    put(root, "MUSIC/Other/c.mp3", &mp3("Charlie", "Other"));
    put(root, "WOID_RECORD/Mic 1.mp3", &mp3("Mic 1", ""));
    let opts = WriteOptions {
        build: BuildOptions { artist_albums: true, ..Default::default() },
        rename_files: true,
        backup_dir: None,
        drive: None,
        dry_run: false,
    };
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    let sopts = ScanOptions { music_dir: String::new(), include_recordings: true, placeholders: true };
    let items =
        neuros_core::scan(fs.as_ref(), info.drive, &sopts, &Default::default(), &AtomicBool::new(false), |_, _| {})
            .unwrap();
    neuros_core::write_database(fs.as_ref(), &info, &items, &opts).unwrap();

    // c.mp3 was deleted by hand: its record must still go. Paths are given
    // in another case than on the device.
    std::fs::remove_file(root.join("MUSIC/Other/c.mp3")).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    let r = neuros_core::dedupe::delete_tracks(
        fs.as_ref(),
        &info,
        &["music/some band/first/a.mp3".into(), "MUSIC/Other/c.mp3".into()],
        &opts,
    )
    .unwrap();
    fs.finish().unwrap();
    assert_eq!(r.deleted, ["MUSIC/Some Band/First/a.mp3"]);
    assert_eq!(r.skipped.len(), 1);
    assert!(!root.join("MUSIC/Some Band/First/a.mp3").exists());
    assert!(root.join("MUSIC/Some Band/First/b.mp3").exists());
    assert!(!root.join("MUSIC/Other").exists(), "empty folder removed");

    let db = load_database(&StdVfs::new(root.to_path_buf()), "WOID_DB/audio", "audio").unwrap();
    assert_eq!(neuros_core::verify::verify(&db), Vec::<String>::new());
    let mut titles: Vec<String> = db.tracks().iter().map(|t| t.title.clone()).collect();
    titles.sort();
    assert_eq!(titles, ["Bravo", "Mic 1"]);
}
