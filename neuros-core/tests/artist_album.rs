use std::collections::BTreeMap;

use neuros_core::reader::{ChildDb, Database, load_database, load_database_from_files};
use neuros_core::verify::{is_note, verify};
use neuros_core::{BuildOptions, Track, build_audio};

fn t(title: &str, artist: Option<&str>, album: Option<&str>) -> Track {
    Track {
        title: title.into(),
        artist: artist.map(Into::into),
        album: album.map(Into::into),
        genre: None,
        recording: None,
        disc: None,
        track_no: None,
        duration_secs: 60,
        size_kb: 1000,
        device_path: format!("C:/MUSIC/{title}.mp3"),
    }
}

fn aa(db: &Database) -> &ChildDb {
    db.children
        .iter()
        .find(|c| c.file_stem == "albums")
        .unwrap()
        .subs
        .iter()
        .find(|s| s.file_stem == "artistalbum")
        .unwrap()
}

/// artist → album names, in menu order.
fn aa_map(db: &Database) -> Vec<(String, Vec<String>)> {
    let albums = db.children.iter().find(|c| c.file_stem == "albums").unwrap();
    let sub = aa(db);
    sub.sai
        .iter()
        .filter_map(|e| {
            let name = sub.mdb.records.get(&e.mdb)?.primary()?;
            let list = sub.pai.get(&e.pai).map(|m| m.entries.clone()).unwrap_or_default();
            Some((name, list.iter().filter_map(|p| albums.mdb.records.get(p)?.primary()).collect()))
        })
        .collect()
}

#[test]
fn artist_album_grouping() {
    let tracks = vec![
        t("a1", Some("Air"), Some("Moon Safari")),
        t("a2", Some("Air"), Some("Talkie Walkie")),
        t("b1", Some("The Beatles"), Some("Abbey Road")),
        t("c1", Some("Air"), Some("Hits Mix")),
        t("c2", Some("Beck"), Some("Hits Mix")), // two artists → compilation
        t("d1", None, Some("Mystery")),          // no artist → not listed
        t("e1", Some("Various Artists"), Some("Now 1")),
    ];
    let files = build_audio(&tracks, &BuildOptions { artist_albums: true, ..Default::default() }).unwrap();
    let db = load_database_from_files(&files, "audio").unwrap();
    assert_eq!(verify(&db), Vec::<String>::new());
    assert_eq!(
        aa_map(&db),
        vec![
            ("Various Artists".to_string(), vec!["Hits Mix".to_string(), "Now 1".to_string()]),
            ("Air".to_string(), vec!["Moon Safari".to_string(), "Talkie Walkie".to_string()]),
            ("The Beatles".to_string(), vec!["Abbey Road".to_string()]),
        ]
    );
    // Albums header carries the "Artists" sub-rule; audio header is the NeurosDBM one.
    let albums = db.children.iter().find(|c| c.file_stem == "albums").unwrap();
    assert_eq!(albums.mdb.rules[1].filename.as_deref(), Some("artistalbum.mdb"));
    assert_eq!(db.mdb.header_len, 0xda);

    // Off: classic positron layout, no artistalbum files.
    let files = build_audio(&tracks, &BuildOptions::default()).unwrap();
    assert!(files.iter().all(|f| !f.name.starts_with("artistalbum")));
    assert_eq!(load_database_from_files(&files, "audio").unwrap().mdb.header_len, 0xcb);
}

/// Rebuild a real device database (copied with `neuros-db backup`) from its
/// own tracks and compare the Artist/Album menu with what NeurosDBM wrote.
/// (User's device, 2026-11-18: 221 of 228 artists identical.)
/// Run with NEUROS_REAL_DB=/path/to/backup (the folder holding `audio/`).
#[test]
fn matches_real_neurosdbm_database() {
    let Some(dir) = std::env::var_os("NEUROS_REAL_DB") else { return };
    let vfs = neuros_core::vfs::StdVfs::new(std::path::PathBuf::from(dir));
    let real = load_database(&vfs, "audio", "audio").unwrap();
    let tracks: Vec<Track> = real
        .tracks()
        .into_iter()
        .map(|e| Track {
            title: e.title,
            artist: e.keys[1].first().cloned(),
            album: e.keys[2].first().cloned(),
            genre: e.keys[3].first().cloned(),
            recording: e.keys[4].first().cloned(),
            disc: None,
            track_no: None,
            duration_secs: e.time,
            size_kb: e.size_kb,
            device_path: e.path,
        })
        .collect();
    let files = build_audio(&tracks, &BuildOptions { artist_albums: true, ..Default::default() }).unwrap();
    let ours = load_database_from_files(&files, "audio").unwrap();
    assert_eq!(verify(&ours), Vec::<String>::new());
    assert_eq!(ours.tracks().len(), tracks.len());
    assert!(verify(&real).iter().all(|p| is_note(p)));

    let as_map = |v: Vec<(String, Vec<String>)>| -> BTreeMap<String, Vec<String>> {
        v.into_iter()
            .map(|(k, mut l)| {
                l.sort_by_key(|s| s.to_lowercase());
                (k.to_lowercase(), l)
            })
            .collect()
    };
    let (theirs, mine) = (as_map(aa_map(&real)), as_map(aa_map(&ours)));
    let mut diffs = Vec::new();
    for k in theirs.keys().chain(mine.keys()).collect::<std::collections::BTreeSet<_>>() {
        if theirs.get(k) != mine.get(k) {
            diffs.push(format!("{k}: real={:?} ours={:?}", theirs.get(k), mine.get(k)));
        }
    }
    eprintln!("{} artists real, {} ours, {} differ", theirs.len(), mine.len(), diffs.len());
    for d in diffs.iter().take(15) {
        eprintln!("  {d}");
    }
    // Differences are expected where NeurosDBM kept case-only duplicates
    // apart (we merge them) or its artistalbum data went stale after later
    // edits by other tools.  The bulk must match.
    let total = theirs.len().max(mine.len());
    assert!(diffs.len() * 20 <= total, "{} of {total} artists differ", diffs.len());
}
