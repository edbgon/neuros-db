//! Empty, known-good database templates (from positron, BSD license).
//! Paths are relative to WOID_DB/.  `audio-aa/` holds the NeurosDBM-style
//! headers (firmware >= 2.14 Artist/Album menu): audio.mdb with its newer
//! menu block, albums.mdb with "All"/"Artists" sub-rules, artistalbum.mdb.

pub static TEMPLATES: &[(&str, &[u8])] = &[
    ("audio-aa/albums.mdb", include_bytes!("../templates/audio-aa/albums.mdb")),
    ("audio-aa/artistalbum.mdb", include_bytes!("../templates/audio-aa/artistalbum.mdb")),
    ("audio-aa/audio.mdb", include_bytes!("../templates/audio-aa/audio.mdb")),
    ("audio/albums.mdb", include_bytes!("../templates/audio/albums.mdb")),
    ("audio/albums.pai", include_bytes!("../templates/audio/albums.pai")),
    ("audio/albums.sai", include_bytes!("../templates/audio/albums.sai")),
    ("audio/artist.mdb", include_bytes!("../templates/audio/artist.mdb")),
    ("audio/artist.pai", include_bytes!("../templates/audio/artist.pai")),
    ("audio/artist.sai", include_bytes!("../templates/audio/artist.sai")),
    ("audio/audio.mdb", include_bytes!("../templates/audio/audio.mdb")),
    ("audio/audio.sai", include_bytes!("../templates/audio/audio.sai")),
    ("audio/genre.mdb", include_bytes!("../templates/audio/genre.mdb")),
    ("audio/genre.pai", include_bytes!("../templates/audio/genre.pai")),
    ("audio/genre.sai", include_bytes!("../templates/audio/genre.sai")),
    ("audio/playlist.mdb", include_bytes!("../templates/audio/playlist.mdb")),
    ("audio/playlist.pai", include_bytes!("../templates/audio/playlist.pai")),
    ("audio/playlist.sai", include_bytes!("../templates/audio/playlist.sai")),
    ("audio/recordings.mdb", include_bytes!("../templates/audio/recordings.mdb")),
    ("audio/recordings.pai", include_bytes!("../templates/audio/recordings.pai")),
    ("audio/recordings.sai", include_bytes!("../templates/audio/recordings.sai")),
    ("failedhisi/failedhisi.mdb", include_bytes!("../templates/failedhisi/failedhisi.mdb")),
    ("failedhisi/failedhisi.sai", include_bytes!("../templates/failedhisi/failedhisi.sai")),
    ("idedhisi/idedhisi.mdb", include_bytes!("../templates/idedhisi/idedhisi.mdb")),
    ("idedhisi/idedhisi.sai", include_bytes!("../templates/idedhisi/idedhisi.sai")),
    ("pcaudio/albums.mdb", include_bytes!("../templates/pcaudio/albums.mdb")),
    ("pcaudio/albums.pai", include_bytes!("../templates/pcaudio/albums.pai")),
    ("pcaudio/albums.sai", include_bytes!("../templates/pcaudio/albums.sai")),
    ("pcaudio/artist.mdb", include_bytes!("../templates/pcaudio/artist.mdb")),
    ("pcaudio/artist.pai", include_bytes!("../templates/pcaudio/artist.pai")),
    ("pcaudio/artist.sai", include_bytes!("../templates/pcaudio/artist.sai")),
    ("pcaudio/genre.mdb", include_bytes!("../templates/pcaudio/genre.mdb")),
    ("pcaudio/genre.pai", include_bytes!("../templates/pcaudio/genre.pai")),
    ("pcaudio/genre.sai", include_bytes!("../templates/pcaudio/genre.sai")),
    ("pcaudio/pcaudio.mdb", include_bytes!("../templates/pcaudio/pcaudio.mdb")),
    ("pcaudio/pcaudio.sai", include_bytes!("../templates/pcaudio/pcaudio.sai")),
    ("pcaudio/playlist.mdb", include_bytes!("../templates/pcaudio/playlist.mdb")),
    ("pcaudio/playlist.pai", include_bytes!("../templates/pcaudio/playlist.pai")),
    ("pcaudio/playlist.sai", include_bytes!("../templates/pcaudio/playlist.sai")),
    ("pcaudio/recordings.mdb", include_bytes!("../templates/pcaudio/recordings.mdb")),
    ("pcaudio/recordings.pai", include_bytes!("../templates/pcaudio/recordings.pai")),
    ("pcaudio/recordings.sai", include_bytes!("../templates/pcaudio/recordings.sai")),
    ("unidedhisi/unidedhisi.mdb", include_bytes!("../templates/unidedhisi/unidedhisi.mdb")),
    ("unidedhisi/unidedhisi.sai", include_bytes!("../templates/unidedhisi/unidedhisi.sai")),
];

pub fn get(path: &str) -> Option<&'static [u8]> {
    TEMPLATES.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)
}

/// Template files belonging to one root database directory (e.g. "pcaudio").
pub fn for_db(db: &str) -> impl Iterator<Item = (&'static str, &'static [u8])> + '_ {
    TEMPLATES
        .iter()
        .filter_map(move |(p, b)| p.strip_prefix(db).and_then(|r| r.strip_prefix('/')).map(|name| (name, *b)))
}

pub const ROOT_DBS: [&str; 5] = ["audio", "pcaudio", "idedhisi", "unidedhisi", "failedhisi"];
