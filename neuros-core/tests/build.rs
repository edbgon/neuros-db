use neuros_core::reader::{load_database_from_files, parse_mdb};
use neuros_core::{BuildOptions, DbFile, Track, build_audio, templates, verify::verify};

fn template_files(db: &str) -> Vec<DbFile> {
    templates::for_db(db).map(|(n, b)| DbFile { name: n.to_string(), bytes: b.to_vec() }).collect()
}

#[test]
fn templates_are_consistent() {
    for db in templates::ROOT_DBS {
        let parsed = load_database_from_files(&template_files(db), db).unwrap();
        let problems: Vec<_> = verify(&parsed)
            .into_iter()
            .filter(|p| !p.contains("isModified")) // templates ship unmodified
            .collect();
        assert!(problems.is_empty(), "{db}: {problems:?}");
    }
}

#[test]
fn empty_build_matches_template_byte_for_byte() {
    let built = build_audio(&[], &BuildOptions::default()).unwrap();
    let tpl = template_files("audio");
    assert_eq!(built.len(), tpl.len());
    for f in &built {
        let t = tpl.iter().find(|t| t.name == f.name).unwrap();
        let mut expect = t.bytes.clone();
        if f.name.ends_with(".mdb") {
            expect[5] |= 1; // we set isModified
        }
        assert_eq!(f.bytes, expect, "{} differs from template", f.name);
    }
}

fn track(title: &str, artist: Option<&str>, album: Option<&str>, n: u32) -> Track {
    Track {
        title: title.into(),
        artist: artist.map(Into::into),
        album: album.map(Into::into),
        genre: Some("Rock".into()),
        recording: None,
        disc: None,
        track_no: Some(n),
        duration_secs: 180 + n,
        size_kb: 4000 + n,
        device_path: format!("C:/MUSIC/{title}.mp3"),
    }
}

#[test]
fn build_roundtrip_and_ordering() {
    let tracks = vec![
        track("Zebra", Some("The Bunch"), Some("Second"), 2),
        track("apple", Some("Acme"), Some("First"), 1),
        track("Mango", Some("acme"), Some("First"), 3), // same artist, other case
        track("Kiwi", None, None, 1),
        track("Banana", Some("Acme"), Some("First"), 2),
    ];
    let files = build_audio(&tracks, &BuildOptions::default()).unwrap();
    let db = load_database_from_files(&files, "audio").unwrap();
    assert_eq!(verify(&db), Vec::<String>::new());

    let got = db.tracks();
    let titles: Vec<_> = got.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["apple", "Banana", "Kiwi", "Mango", "Zebra"]);
    let mango = got.iter().find(|t| t.title == "Mango").unwrap();
    assert_eq!(mango.keys[1], ["Acme"]); // merged case-insensitively
    assert_eq!(mango.time, 183);
    assert_eq!(mango.size_kb, 4003);
    assert_eq!(mango.path, "C:/MUSIC/Mango.mp3");
    let kiwi = got.iter().find(|t| t.title == "Kiwi").unwrap();
    assert!(kiwi.keys[1].is_empty() && kiwi.keys[0].is_empty());

    // Artist list order: Acme before "The Bunch" (sorted as "Bunch").
    let artist = &db.children[1];
    let names: Vec<_> = artist.sai.iter().filter_map(|e| artist.mdb.records[&e.mdb].primary()).collect();
    assert_eq!(names, ["Acme", "The Bunch"]);

    // Album "First" lists tracks by track number: apple(1), Banana(2), Mango(3).
    let albums = &db.children[2];
    let first = albums.sai.iter().find(|e| albums.mdb.records[&e.mdb].primary().as_deref() == Some("First")).unwrap();
    let order: Vec<_> = albums.pai[&first.pai].entries.iter().map(|p| db.mdb.records[p].primary().unwrap()).collect();
    assert_eq!(order, ["apple", "Banana", "Mango"]);

    let root = parse_mdb(&files.iter().find(|f| f.name == "audio.mdb").unwrap().bytes, "audio").unwrap();
    assert_eq!(root.status & 1, 1);
}

#[test]
fn large_build_exercises_escaping() {
    // Thousands of tracks/artists so pointers and sizes hit 0x23/0x24/0x25/0x2f words.
    let tracks: Vec<Track> = (0..6000u32)
        .map(|i| Track {
            title: format!("T{i}"),
            artist: Some(format!("A{}", i % 700)),
            album: Some(format!("L{}", i % 1300)),
            genre: Some(format!("G{}", i % 40)),
            recording: if i % 50 == 0 { Some("Microphone".into()) } else { None },
            disc: None,
            track_no: Some(i % 15),
            duration_secs: i % 64,    // includes 0x23.. 0x2f
            size_kb: 0x0023_0000 + i, // high word special
            device_path: format!("C:/MUSIC/{i}.mp3"),
        })
        .collect();
    let files = build_audio(&tracks, &BuildOptions::default()).unwrap();
    let db = load_database_from_files(&files, "audio").unwrap();
    assert_eq!(verify(&db), Vec::<String>::new());
    let got = db.tracks();
    assert_eq!(got.len(), 6000);
    for t in &got {
        let i: u32 = t.title[1..].parse().unwrap();
        assert_eq!(t.time, i % 64);
        assert_eq!(t.size_kb, 0x0023_0000 + i);
        assert_eq!(t.keys[1], [format!("A{}", i % 700)]);
    }
}
