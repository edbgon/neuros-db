use std::path::Path;
use std::sync::atomic::AtomicBool;

use neuros_core::dedupe::{Relation, delete_tracks, find_duplicates};
use neuros_core::reader::load_database;
use neuros_core::vfs::StdVfs;
use neuros_core::{BuildOptions, ScanOptions, Source, WriteOptions};

/// A tiny MP3; `tag` = (title, artist, album) as ID3v1, or untagged.
fn mp3(tag: Option<(&str, &str, &str)>, frames: usize, salt: u8) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..frames {
        v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        v.extend(std::iter::repeat_n(salt, 417 - 4));
    }
    if let Some((title, artist, album)) = tag {
        let mut t = vec![0u8; 128];
        t[..3].copy_from_slice(b"TAG");
        for (off, s) in [(3, title), (33, artist), (63, album)] {
            t[off..off + s.len()].copy_from_slice(s.as_bytes());
        }
        v.extend(t);
    }
    v
}

fn put(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

fn wopts() -> WriteOptions {
    WriteOptions {
        build: BuildOptions { artist_albums: true, ..Default::default() },
        rename_files: true,
        backup_dir: None,
        drive: None,
        dry_run: false,
    }
}

#[test]
fn finds_and_removes_duplicates() {
    let dev = tempfile::tempdir().unwrap();
    let root = dev.path();
    std::fs::write(root.join("sn.txt"), "1").unwrap();
    let br = |t| Some((t, "Some Band", "Early"));
    // The same file twice.
    put(root, "MUSIC/Synth Co/Misc/Robot Remix.mp3", &mp3(Some(("Robot Remix", "Synth Co", "Misc")), 40, 1));
    put(root, "MUSIC/Synth Co [NEW]/Robot remix.mp3", &mp3(Some(("Robot Remix", "Synth Co", "Misc")), 40, 1));
    // Two rips of one album (different bytes), with a song that is on the
    // album twice (tracks 15 and 26) — those two are not copies.
    put(root, "MUSIC/Some Band/Early/10._Night_Drive.mp3", &mp3(br("Night Drive"), 50, 2));
    put(root, "MUSIC/Some Band/Early - 1990/10._Night_Drive.mp3", &mp3(br("Night Drive"), 51, 3));
    put(root, "MUSIC/Some Band/Early/15._Some_Band.mp3", &mp3(br("Some Band"), 60, 4));
    put(root, "MUSIC/Some Band/Early/26._Some_Band.mp3", &mp3(br("Some Band"), 61, 5));
    put(root, "MUSIC/Some Band/Early - 1990/15._Some_Band.mp3", &mp3(br("Some Band"), 62, 6));
    put(root, "MUSIC/Some Band/Early - 1990/26._Some_Band.mp3", &mp3(br("Some Band"), 63, 7));
    // A live version is a different song.
    put(
        root,
        "MUSIC/Some Band/Grey Skies/15._Rainy Day.mp3",
        &mp3(Some(("Rainy Day", "Some Band", "Grey Skies")), 50, 8),
    );
    put(
        root,
        "MUSIC/Some Band/Live/20._Rainy Day_(live).mp3",
        &mp3(Some(("Rainy Day (live)", "Some Band", "Live Years")), 50, 9),
    );
    // An untagged older copy next to a tagged one.
    put(
        root,
        "MUSIC/The Testers!/Picnic Basket/1._BLT.mp3",
        &mp3(Some(("B.L.T. (Bacon Lettuce Tomato)", "The Testers!", "Picnic Basket")), 45, 10),
    );
    put(root, "MUSIC/The Testers!/Picnic Basket - 2003/1 - The Testers! - B.L.T..mp3", &mp3(None, 44, 11));
    // Same song on a best-of.
    put(
        root,
        "MUSIC/The Examples/Second Album/5._Hello There.mp3",
        &mp3(Some(("Hello There", "The Examples", "Second Album")), 50, 12),
    );
    put(
        root,
        "MUSIC/The Examples/Best of the Examples/1._Hello There.mp3",
        &mp3(Some(("Hello There", "The Examples", "Best of the Examples")), 50, 13),
    );
    // A recording is never a duplicate.
    put(root, "WOID_RECORD/Robot Remix.mp3", &mp3(Some(("Robot Remix", "Synth Co", "Misc")), 40, 1));

    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    let sopts = ScanOptions { music_dir: String::new(), include_recordings: true, placeholders: true };
    let items =
        neuros_core::scan(fs.as_ref(), info.drive, &sopts, &Default::default(), &AtomicBool::new(false), |_, _| {})
            .unwrap();
    neuros_core::write_database(fs.as_ref(), &info, &items, &wopts()).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);

    let show = |groups: &[neuros_core::dedupe::DupGroup]| -> Vec<Vec<(String, Relation)>> {
        groups.iter().map(|g| g.files.iter().map(|f| (f.path.clone(), f.relation)).collect()).collect()
    };
    let groups = find_duplicates(fs.as_ref(), &info, false, &AtomicBool::new(false), |_, _| {}).unwrap();
    let got = show(&groups);
    let has = |a: &str, b: &str, rel: Relation| {
        got.iter().any(|g| {
            g.len() == 2
                && g.iter().any(|(p, r)| p.ends_with(a) && *r == Relation::Keeper)
                && g.iter().any(|(p, r)| p.ends_with(b) && *r == rel)
        })
    };
    assert_eq!(got.len(), 5, "{got:#?}");
    assert!(
        has("Synth Co/Misc/Robot Remix.mp3", "Synth Co [NEW]/Robot remix.mp3", Relation::Identical)
            || has("Synth Co [NEW]/Robot remix.mp3", "Synth Co/Misc/Robot Remix.mp3", Relation::Identical),
        "{got:#?}"
    );
    let pair = |a: &str, b: &str| has(a, b, Relation::SameAlbum) || has(b, a, Relation::SameAlbum);
    assert!(pair("Early/10._Night_Drive.mp3", "Early - 1990/10._Night_Drive.mp3"), "{got:#?}");
    assert!(pair("Early/15._Some_Band.mp3", "Early - 1990/15._Some_Band.mp3"), "{got:#?}");
    assert!(pair("Early/26._Some_Band.mp3", "Early - 1990/26._Some_Band.mp3"), "{got:#?}");
    // The tagged copy is kept over the untagged one.
    assert!(has("Picnic Basket/1._BLT.mp3", "1 - The Testers! - B.L.T..mp3", Relation::SameAlbum), "{got:#?}");

    // The best-of copy only shows up when asked for.
    let all = show(&find_duplicates(fs.as_ref(), &info, true, &AtomicBool::new(false), |_, _| {}).unwrap());
    assert_eq!(all.len(), 6);
    assert!(all.iter().any(|g| g.iter().any(|(p, r)| p.ends_with("Hello There.mp3") && *r == Relation::OtherAlbum)));

    // Delete the suggested extras.
    let doomed: Vec<String> =
        groups.iter().flat_map(|g| &g.files).filter(|f| f.relation.suggest_delete()).map(|f| f.path.clone()).collect();
    assert_eq!(doomed.len(), 5);
    let r = delete_tracks(fs.as_ref(), &info, &doomed, &wopts()).unwrap();
    fs.finish().unwrap();
    assert_eq!(r.deleted.len(), 5);
    assert_eq!(r.tracks_written, 15 - 5);
    for p in &doomed {
        assert!(!root.join(p).exists(), "{p}");
    }
    assert!(
        !root.join("MUSIC/Synth Co [NEW]").exists() || !root.join("MUSIC/Synth Co/Misc").exists(),
        "emptied folder removed"
    );
    let db = load_database(&StdVfs::new(root.to_path_buf()), "WOID_DB/audio", "audio").unwrap();
    assert_eq!(neuros_core::verify::verify(&db), Vec::<String>::new());
    assert_eq!(db.tracks().len(), 10);
    assert!(root.join("WOID_RECORD/Robot Remix.mp3").exists());
}

#[test]
fn sync_does_not_bring_back_removed_duplicates() {
    use neuros_core::sync::{SyncAction, apply_sync, plan_sync};
    let dev = tempfile::tempdir().unwrap();
    let pc = tempfile::tempdir().unwrap();
    let root = dev.path();
    std::fs::write(root.join("sn.txt"), "1").unwrap();
    let br = |t| Some((t, "Some Band", "Early"));
    // The PC has the album twice (two rips) plus one more album.
    for (dir, salt) in [("Early - 1990", 2u8), ("Early Years Remaster - 1990", 3)] {
        put(
            pc.path(),
            &format!("Some Band/{dir}/10._Night_Drive.mp3"),
            &mp3(br("Night Drive"), 50 + salt as usize, salt),
        );
        put(
            pc.path(),
            &format!("Some Band/{dir}/11._Cold_Coffee.mp3"),
            &mp3(br("Cold Coffee"), 40 + salt as usize, salt),
        );
    }
    put(
        pc.path(),
        "The Examples/Second Album/5._Hello There.mp3",
        &mp3(Some(("Hello There", "The Examples", "Second Album")), 50, 9),
    );
    let sopts = ScanOptions { music_dir: String::new(), include_recordings: true, placeholders: true };
    let plan_and_sync = |skip: bool| {
        let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
        let info = neuros_core::inspect(fs.as_ref(), None);
        let known: Vec<_> =
            info.existing.as_ref().map(|db| db.tracks().iter().map(|t| t.to_track()).collect()).unwrap_or_default();
        let plan =
            plan_sync(fs.as_ref(), &known, pc.path(), "MUSIC", true, skip, &AtomicBool::new(false), |_, _| {}).unwrap();
        let r = apply_sync(fs.as_ref(), &info, &plan, false, &wopts(), &sopts, |_, _| {}).unwrap();
        fs.finish().unwrap();
        (plan, r)
    };

    // Empty Neuros: only one rip of the album is copied.
    let (plan, r) = plan_and_sync(true);
    assert_eq!(plan.count(SyncAction::Skip).0, 2);
    assert_eq!(r.added.len(), 3);
    assert!(
        plan.entries.iter().filter(|e| e.action == SyncAction::Skip).all(|e| e
            .dup_of
            .as_deref()
            .unwrap()
            .contains("/Early - 1990/"))
    );

    // Without skipping, the second rip arrives; the deduper finds it, and
    // after deleting it the next (skipping) sync doesn't copy it back.
    let (_, r) = plan_and_sync(false);
    assert_eq!(r.added.len(), 2);
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    let groups = find_duplicates(fs.as_ref(), &info, false, &AtomicBool::new(false), |_, _| {}).unwrap();
    assert_eq!(groups.len(), 2);
    let doomed: Vec<String> =
        groups.iter().flat_map(|g| &g.files).filter(|f| f.relation.suggest_delete()).map(|f| f.path.clone()).collect();
    delete_tracks(fs.as_ref(), &info, &doomed, &wopts()).unwrap();
    fs.finish().unwrap();
    let (plan, r) = plan_and_sync(true);
    assert!(r.added.is_empty(), "{:?}", r.added);
    assert_eq!(plan.count(SyncAction::Skip).0, 2);
    let db = load_database(&StdVfs::new(root.to_path_buf()), "WOID_DB/audio", "audio").unwrap();
    assert_eq!(neuros_core::verify::verify(&db), Vec::<String>::new());
    assert_eq!(db.tracks().len(), 3);
}
