use std::sync::atomic::AtomicBool;

use neuros_core::reader::load_database;
use neuros_core::vfs::StdVfs;
use neuros_core::{BuildOptions, ScanOptions, Source, WriteOptions};

fn mp3(title: &str, artist: &str, album: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..20 {
        v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        v.extend(std::iter::repeat_n(0u8, 413));
    }
    let mut t = vec![0u8; 128];
    t[..3].copy_from_slice(b"TAG");
    for (off, s) in [(3, title), (33, artist), (63, album)] {
        t[off..off + s.len()].copy_from_slice(s.as_bytes());
    }
    v.extend(t);
    v
}

fn wopts(dry_run: bool) -> WriteOptions {
    WriteOptions {
        build: BuildOptions { artist_albums: true, ..Default::default() },
        rename_files: true,
        backup_dir: None,
        drive: None,
        dry_run,
    }
}

#[test]
fn repairs_existing_database_and_new_scans() {
    let dev = tempfile::tempdir().unwrap();
    let root = dev.path();
    std::fs::write(root.join("sn.txt"), "1").unwrap();
    std::fs::create_dir_all(root.join("MUSIC")).unwrap();
    std::fs::write(root.join("MUSIC/a.mp3"), mp3("Don?t Cry", "Man or Astro?Man?", "80?85")).unwrap();
    std::fs::write(root.join("MUSIC/b.mp3"), mp3("Where Is My Mind?", "Pixies", "Surfer Rosa")).unwrap();

    // A database written by an older tool, with the "?" in it.
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    let sopts = ScanOptions { music_dir: String::new(), include_recordings: true, placeholders: true };
    let mut items =
        neuros_core::scan(fs.as_ref(), info.drive, &sopts, &Default::default(), &AtomicBool::new(false), |_, _| {})
            .unwrap();
    // New scans already repair tags...
    let mut titles: Vec<_> = items.iter().map(|i| i.track.title.clone()).collect();
    titles.sort();
    assert_eq!(titles, ["Don't Cry", "Where Is My Mind?"]);
    // ...so put the broken text back to imitate the old database.
    for it in &mut items {
        if it.track.title == "Don't Cry" {
            it.track.title = "Don?t Cry".into();
            it.track.artist = Some("Man or Astro?Man?".into());
            it.track.album = Some("80?85".into());
        }
    }
    neuros_core::write_database(fs.as_ref(), &info, &items, &wopts(false)).unwrap();

    let info = neuros_core::inspect(fs.as_ref(), None);
    let (changes, _) = neuros_core::textfix::fix_database(fs.as_ref(), &info, &wopts(true)).unwrap();
    assert_eq!(changes.len(), 3);
    let db = load_database(&StdVfs::new(root.to_path_buf()), "WOID_DB/audio", "audio").unwrap();
    assert!(db.tracks().iter().any(|t| t.title == "Don?t Cry"), "dry run wrote nothing");

    let (changes, r) = neuros_core::textfix::fix_database(fs.as_ref(), &info, &wopts(false)).unwrap();
    fs.finish().unwrap();
    assert_eq!(changes.len(), 3);
    assert_eq!(r.tracks_written, 2);
    let db = load_database(&StdVfs::new(root.to_path_buf()), "WOID_DB/audio", "audio").unwrap();
    assert_eq!(neuros_core::verify::verify(&db), Vec::<String>::new());
    let mut got: Vec<String> =
        db.tracks().iter().map(|t| format!("{}|{}|{}", t.title, t.keys[1].join(","), t.keys[2].join(","))).collect();
    got.sort();
    assert_eq!(got, ["Don't Cry|Man or Astro-Man?|80-85", "Where Is My Mind?|Pixies|Surfer Rosa"]);
    // Nothing left to fix.
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    assert!(neuros_core::textfix::fix_database(fs.as_ref(), &info, &wopts(true)).unwrap().0.is_empty());
}
