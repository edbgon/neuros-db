use std::path::Path;
use std::sync::atomic::AtomicBool;

use neuros_core::overrides::{Field, Overrides};
use neuros_core::reader::load_database;
use neuros_core::sync::{SyncAction, apply_sync, plan_sync};
use neuros_core::vfs::{StdVfs, Vfs};
use neuros_core::{BuildOptions, ScanOptions, Source, WriteOptions};

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

fn wopts() -> WriteOptions {
    WriteOptions {
        build: BuildOptions { artist_albums: true, ..Default::default() },
        rename_files: true,
        backup_dir: None,
        drive: None,
        dry_run: false,
    }
}

fn sopts() -> ScanOptions {
    ScanOptions { music_dir: String::new(), include_recordings: true, placeholders: true }
}

fn titles(root: &Path) -> Vec<String> {
    let db = load_database(&StdVfs::new(root.to_path_buf()), "WOID_DB/audio", "audio").unwrap();
    assert_eq!(neuros_core::verify::verify(&db), Vec::<String>::new());
    let mut t: Vec<_> = db.tracks().into_iter().map(|t| format!("{}|{}", t.title, t.keys[1].join(","))).collect();
    t.sort();
    t
}

fn known(info: &neuros_core::DeviceInfo) -> Vec<neuros_core::Track> {
    info.existing.as_ref().map(|db| db.tracks().iter().map(|t| t.to_track()).collect()).unwrap_or_default()
}

fn sync(root: &Path, pc: &Path, delete: bool) -> neuros_core::WriteReport {
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    let plan =
        plan_sync(fs.as_ref(), &known(&info), pc, "MUSIC", true, true, &AtomicBool::new(false), |_, _| {}).unwrap();
    let r = apply_sync(fs.as_ref(), &info, &plan, delete, &wopts(), &sopts(), |_, _| {}).unwrap();
    fs.finish().unwrap();
    r
}

#[test]
fn sync_then_edit_and_edits_survive_rebuilds() {
    let dev = tempfile::tempdir().unwrap();
    let pc = tempfile::tempdir().unwrap();
    let root = dev.path();
    std::fs::write(root.join("sn.txt"), "1").unwrap();
    // On the Neuros already: one file that is also on the PC (same), one
    // only on the Neuros, and a recording outside MUSIC.
    put(&root.join("MUSIC/ABBA/Gold/SOS.mp3"), &fake_mp3("SOS", "ABBA", "Gold", 2, 50));
    put(&root.join("MUSIC/Old/gone.mp3"), &fake_mp3("Gone", "Old", "Old", 1, 20));
    put(&root.join("WOID_RECORD/Mic 1.mp3"), &fake_mp3("Mic 1", "", "", 0, 10));
    // On the PC.
    put(&pc.path().join("ABBA/Gold/SOS.mp3"), &fake_mp3("SOS", "ABBA", "Gold", 2, 50));
    put(&pc.path().join("ABBA/Gold/DQ.mp3"), &fake_mp3("Dancing Queen", "ABBA", "Gold", 1, 60));
    put(&pc.path().join("Björk/Jóga.mp3"), &fake_mp3("Joga", "Bjork", "Homogenic", 1, 30));

    // Plan.
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(false).unwrap();
    let plan = plan_sync(fs.as_ref(), &[], pc.path(), "music", true, true, &AtomicBool::new(false), |_, _| {}).unwrap();
    assert_eq!(plan.device_dir, "MUSIC");
    assert_eq!(plan.count(SyncAction::Copy).0, 2);
    assert_eq!(plan.count(SyncAction::Unchanged).0, 1);
    assert_eq!(
        plan.extra,
        [("MUSIC/Old/gone.mp3".to_string(), std::fs::metadata(root.join("MUSIC/Old/gone.mp3")).unwrap().len())]
    );
    assert!(plan.entries.iter().any(|e| e.dest == "MUSIC/Bjork/Joga.mp3"));
    drop(fs);

    // Sync without deleting: no database yet, so the device is scanned first.
    let r = sync(root, pc.path(), false);
    assert_eq!(r.added.len(), 2);
    assert!(root.join("MUSIC/Old/gone.mp3").exists());
    assert_eq!(titles(root), ["Dancing Queen|ABBA", "Gone|Old", "Joga|Bjork", "Mic 1|", "SOS|ABBA"]);

    // Change a file on the PC (re-tag + size change) and sync with delete.
    put(&pc.path().join("ABBA/Gold/SOS.mp3"), &fake_mp3("S.O.S.", "ABBA", "Gold", 2, 55));
    let r = sync(root, pc.path(), true);
    assert_eq!(r.added, ["MUSIC/ABBA/Gold/SOS.mp3"]);
    assert_eq!(r.deleted, ["MUSIC/Old/gone.mp3"]);
    assert!(!root.join("MUSIC/Old").exists(), "empty folder removed");
    assert_eq!(titles(root), ["Dancing Queen|ABBA", "Joga|Bjork", "Mic 1|", "S.O.S.|ABBA"]);

    // Edit: rename an artist on two tracks, fix a title.
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    let mut ed = info.overrides.clone();
    for p in ["C:/MUSIC/ABBA/Gold/DQ.mp3", "C:/MUSIC/ABBA/Gold/SOS.mp3"] {
        ed.set(p, Field::Artist, "ABBA (Sweden)");
    }
    ed.set("C:/MUSIC/Bjork/Joga.mp3", Field::Title, "Jóga");
    let r = neuros_core::device::save_edits(fs.as_ref(), &info, &ed, &wopts()).unwrap();
    fs.finish().unwrap();
    assert_eq!(r.edited_tracks, 3);
    let expect = ["Dancing Queen|ABBA (Sweden)", "Joga|Bjork", "Mic 1|", "S.O.S.|ABBA (Sweden)"];
    assert_eq!(titles(root), expect); // "Jóga" stored ASCII-safe

    // Edits survive a re-sync and a full rescan + write.
    sync(root, pc.path(), true);
    assert_eq!(titles(root), expect);
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    assert_eq!(info.overrides.len(), 3);
    let items =
        neuros_core::scan(fs.as_ref(), info.drive, &sopts(), &Default::default(), &AtomicBool::new(false), |_, _| {})
            .unwrap();
    neuros_core::write_database(fs.as_ref(), &info, &items, &wopts()).unwrap();
    assert_eq!(titles(root), expect);

    // Clearing the edits goes back to the file tags.
    let info = neuros_core::inspect(fs.as_ref(), None);
    let mut ed = info.overrides.clone();
    for p in ["C:/MUSIC/ABBA/Gold/DQ.mp3", "C:/MUSIC/ABBA/Gold/SOS.mp3", "C:/MUSIC/Bjork/Joga.mp3"] {
        ed.clear(p);
    }
    assert!(ed.is_empty());
    let items =
        neuros_core::scan(fs.as_ref(), info.drive, &sopts(), &Default::default(), &AtomicBool::new(false), |_, _| {})
            .unwrap();
    neuros_core::device::save_edits(fs.as_ref(), &info, &ed, &wopts()).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    neuros_core::write_database(fs.as_ref(), &info, &items, &wopts()).unwrap();
    assert_eq!(titles(root), ["Dancing Queen|ABBA", "Joga|Bjork", "Mic 1|", "S.O.S.|ABBA"]);
    assert!(!root.join("WOID_DB").join(neuros_core::overrides::EDITS_FILE).exists());
    let _ = Overrides::default();
    let _ = StdVfs::new(root.to_path_buf()).describe();
}

#[test]
fn sync_matches_renamed_and_retagged_files() {
    let dev = tempfile::tempdir().unwrap();
    let pc = tempfile::tempdir().unwrap();
    let root = dev.path();
    std::fs::write(root.join("sn.txt"), "1").unwrap();
    // The Neuros got its files from an older tool with other names.
    put(&root.join("MUSIC/ABBA/1 - ABBA - SOS.mp3"), &fake_mp3("SOS", "ABBA", "Gold", 2, 50));
    put(&root.join("MUSIC/ABBA/2 - ABBA - Waterloo.mp3"), &fake_mp3("Waterloo", "ABBA", "Gold", 3, 40));
    // Same size, different audio: must not be paired by size alone.
    let mut other = fake_mp3("Decoy", "X", "Y", 1, 30);
    let n = other.len();
    other[n / 2] = 1;
    put(&root.join("MUSIC/X/other.mp3"), &other);
    // Write a database for the device.
    let (fs, _) = Source::Mounted(root.to_path_buf()).open(true).unwrap();
    let info = neuros_core::inspect(fs.as_ref(), None);
    let items =
        neuros_core::scan(fs.as_ref(), info.drive, &sopts(), &Default::default(), &AtomicBool::new(false), |_, _| {})
            .unwrap();
    neuros_core::write_database(fs.as_ref(), &info, &items, &wopts()).unwrap();

    // PC: same SOS (other name), Waterloo re-tagged (size changes), and a
    // file the same size as other.mp3 but different audio.
    put(&pc.path().join("ABBA/Gold/02_SOS.mp3"), &fake_mp3("SOS", "ABBA", "Gold", 2, 50));
    put(&pc.path().join("ABBA/Gold/03_Waterloo.mp3"), &fake_mp3("Waterloo", "ABBA", "Gold", 3, 41));
    put(&pc.path().join("X/new.mp3"), &fake_mp3("Other", "X", "Y", 1, 30));

    let info = neuros_core::inspect(fs.as_ref(), None);
    let plan =
        plan_sync(fs.as_ref(), &known(&info), pc.path(), "MUSIC", true, true, &AtomicBool::new(false), |_, _| {})
            .unwrap();
    let find = |name: &str| plan.entries.iter().find(|e| e.host.ends_with(name)).unwrap();
    use neuros_core::sync::Match;
    let sos = find("02_SOS.mp3");
    assert_eq!(
        (sos.action, sos.matched, sos.dest.as_str()),
        (SyncAction::Unchanged, Match::Content, "MUSIC/ABBA/1 - ABBA - SOS.mp3")
    );
    let wat = find("03_Waterloo.mp3");
    assert_eq!(
        (wat.action, wat.matched, wat.dest.as_str()),
        (SyncAction::Update, Match::Tags, "MUSIC/ABBA/2 - ABBA - Waterloo.mp3")
    );
    let new = find("new.mp3");
    assert_eq!((new.action, new.matched), (SyncAction::Copy, Match::None));
    assert_eq!(plan.extra.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["MUSIC/X/other.mp3"]);

    // Applying it overwrites Waterloo in place and doesn't duplicate SOS.
    let r = apply_sync(fs.as_ref(), &info, &plan, false, &wopts(), &sopts(), |_, _| {}).unwrap();
    fs.finish().unwrap();
    let mut added = r.added.clone();
    added.sort();
    assert_eq!(added, ["MUSIC/ABBA/2 - ABBA - Waterloo.mp3", "MUSIC/X/new.mp3"]);
    assert_eq!(titles(root), ["Decoy|X", "Other|X", "SOS|ABBA", "Waterloo|ABBA"]);
    assert!(!root.join("MUSIC/ABBA/Gold").exists());
}

#[test]
fn add_folders_recursively_keeping_structure() {
    use neuros_core::AddLayout;
    let pc = tempfile::tempdir().unwrap();
    put(&pc.path().join("Beck/Odelay/01 Devils Haircut.mp3"), &fake_mp3("Devils Haircut", "Beck", "Odelay", 1, 5));
    put(&pc.path().join("Beck/Odelay/cover.jpg"), b"x");
    put(&pc.path().join("Beck/Mellow Gold/Loser.ogg"), b"not really ogg");
    put(&pc.path().join("single.mp3"), &fake_mp3("Single", "S", "S", 1, 5));
    let sel = [pc.path().join("Beck"), pc.path().join("single.mp3"), pc.path().join("Beck/Odelay")];
    let mut dests: Vec<_> = neuros_core::plan_add(&sel, &AddLayout::KeepFolders("MUSIC".into()), "MUSIC", true)
        .into_iter()
        .map(|p| p.dest)
        .collect();
    dests.sort();
    assert_eq!(
        dests,
        ["MUSIC/Beck/Mellow Gold/Loser.ogg", "MUSIC/Beck/Odelay/01 Devils Haircut.mp3", "MUSIC/single.mp3"]
    );
}
